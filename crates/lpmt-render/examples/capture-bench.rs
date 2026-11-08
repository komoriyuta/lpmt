//! Real-GPU correctness + upload microbenchmark, no desktop permission required.
//! cargo run --release -p lpmt-render --example capture-bench --locked
use anyhow::Result;
use lpmt_media::{CaptureFrame, CapturePixelFormat, DecodedImage};
use lpmt_render::{
    capture::{CaptureTexture, CaptureUploader},
    gpu::Gpu,
    media::MediaCache,
};
use std::{
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};

fn wait(gpu: &Gpu) -> Result<()> {
    gpu.device().poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: Some(Duration::from_secs(30)),
    })?;
    Ok(())
}

fn readback(gpu: &Gpu, frame: &CaptureTexture) -> Result<Vec<u8>> {
    let texture = &frame.texture;
    let pitch = (texture.width() * 4).div_ceil(256) * 256;
    let buffer = gpu.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("capture-check"),
        size: u64::from(pitch) * u64::from(texture.height()),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu.device().create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(pitch),
                rows_per_image: None,
            },
        },
        texture.size(),
    );
    gpu.queue().submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.map_async(wgpu::MapMode::Read, .., move |result| {
        tx.send(result).unwrap();
    });
    wait(gpu)?;
    rx.recv()??;
    let view = buffer.get_mapped_range(..)?;
    let mut rgba = Vec::new();
    for row in view.chunks(pitch as usize) {
        rgba.extend_from_slice(&row[..texture.width() as usize * 4]);
    }
    if texture.format() == wgpu::TextureFormat::Bgra8Unorm {
        for pixel in rgba.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    }
    Ok(rgba)
}

fn check(gpu: &Gpu) -> Result<()> {
    let mut upload = CaptureUploader::new(gpu);
    for format in [
        CapturePixelFormat::Rgba,
        CapturePixelFormat::Bgra,
        CapturePixelFormat::Rgbx,
        CapturePixelFormat::Bgrx,
    ] {
        // Distinct channels, meaningful alpha, padded rows and an offset crop.
        let mut bytes = [99; 32];
        let expected = [
            12, 34, 56, 78, 90, 123, 234, 9, 1, 2, 3, 4, 200, 180, 160, 140,
        ];
        for (i, pixel) in expected.chunks_exact(4).enumerate() {
            let p = &mut bytes[4 + (i / 2) * 16 + (i % 2) * 4..][..4];
            p.copy_from_slice(pixel);
            if matches!(format, CapturePixelFormat::Bgra | CapturePixelFormat::Bgrx) {
                p.swap(0, 2);
            }
        }
        let frame = CaptureFrame {
            width: 2,
            height: 2,
            stride: 16,
            format,
            data: &bytes[4..28],
        };
        let first = upload.upload(0, frame)?;
        let second = upload.upload(0, frame)?;
        assert!(
            Arc::ptr_eq(&first, &second),
            "Stable textures must be reused"
        );
        let mut expected = expected.to_vec();
        if matches!(format, CapturePixelFormat::Rgbx | CapturePixelFormat::Bgrx) {
            for pixel in expected.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
        }
        assert_eq!(
            readback(gpu, &second)?,
            expected,
            "{format:?}: channels/alpha/stride/crop"
        );
        let resized = upload.upload(0, CaptureFrame { width: 1, ..frame })?;
        assert!(!Arc::ptr_eq(&first, &resized));
        assert_eq!(
            readback(gpu, &resized)?,
            [expected[..4].to_vec(), expected[8..12].to_vec()].concat()
        );
        // Retained old frames remain valid after replacement.
        assert_eq!(readback(gpu, &first)?, expected);
    }
    println!("GPU correctness: RGBA/BGRA/RGBx/BGRx, stride, crop, alpha, reuse and resize passed");
    Ok(())
}

// Matches the previous PipeWire pack_frame (allocate, crop-copy, CPU swizzle),
// then the same shared media texture update. This is a release-to-release A/B.
fn legacy(gpu: &Gpu, media: &mut MediaCache, index: usize, frame: CaptureFrame<'_>) -> Result<()> {
    let mut rgba = Vec::with_capacity(frame.width as usize * frame.height as usize * 4);
    for y in 0..frame.height as usize {
        rgba.extend_from_slice(
            &frame.data[y * frame.stride as usize..][..frame.width as usize * 4],
        );
    }
    for p in rgba.chunks_exact_mut(4) {
        p.swap(0, 2);
        p[3] = 255;
    }
    media.update_image(
        gpu,
        lpmt_core::model::MediaId::new(format!("bench-{index}")),
        &DecodedImage::from_rgba8(frame.width, frame.height, rgba)?,
    )?;
    gpu.queue().submit([]);
    Ok(())
}

fn bench(gpu: &Gpu, width: u32, height: u32, optimized: bool) -> Result<(f64, f64, f64)> {
    let mut uploader = CaptureUploader::new(gpu);
    let mut media = MediaCache::new(gpu)?;
    let data: Vec<_> = (0..width as usize * height as usize * 4)
        .map(|i| (i.wrapping_mul(79) % 251) as u8)
        .collect();
    let frame = CaptureFrame {
        width,
        height,
        stride: width * 4,
        format: CapturePixelFormat::Bgrx,
        data: &data,
    };
    let mut cpu = Vec::new();
    let mut completed = Vec::new();
    for n in 0..140 {
        let start = Instant::now();
        for index in 0..2 {
            if optimized {
                black_box(uploader.upload(index, frame)?);
            } else {
                legacy(gpu, &mut media, index, frame)?;
            }
        }
        let cpu_ms = start.elapsed().as_secs_f64() * 1000.0;
        wait(gpu)?;
        if n >= 20 {
            cpu.push(cpu_ms);
            completed.push(start.elapsed().as_secs_f64() * 1000.0);
        }
    }
    cpu.sort_by(f64::total_cmp);
    completed.sort_by(f64::total_cmp);
    Ok((
        cpu[cpu.len() / 2],
        cpu[cpu.len() * 95 / 100],
        completed[completed.len() / 2],
    ))
}

fn main() -> Result<()> {
    let gpu = Gpu::new(wgpu::Instance::new(
        wgpu::InstanceDescriptor::new_without_display_handle(),
    ))?;
    println!(
        "GPU: {} ({:?})",
        gpu.adapter_info().name,
        gpu.adapter_info().backend
    );
    check(&gpu)?;
    if std::env::args().any(|arg| arg == "--check-only") {
        return Ok(());
    }
    println!(
        "Two inputs per sample; 20 warmup + 120 measured; milliseconds; no compositor/display"
    );
    println!("size,round,path,cpu_median_ms,cpu_p95_ms,gpu_completed_median_ms");
    for (width, height) in [(1920, 1080), (3840, 2160)] {
        for round in 0..3 {
            for optimized in if round % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let (median, p95, completed) = bench(&gpu, width, height, optimized)?;
                println!(
                    "{width}x{height},{round},{},{median:.3},{p95:.3},{completed:.3}",
                    if optimized {
                        "direct_gpu"
                    } else {
                        "legacy_cpu"
                    }
                );
            }
        }
    }
    Ok(())
}
