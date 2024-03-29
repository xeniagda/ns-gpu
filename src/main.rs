use std::io::Cursor;

use wgpu::{
    Adapter, Device,
    Buffer,
};

use image::ImageEncoder;

use futures::executor::block_on;

const SHADER_POSSION_RHS: &[u8] = include_bytes!("../compiled-shaders/pressure_poisson_rhs-comp.spv");
const SHADER_SETUP_V: &[u8] = include_bytes!("../compiled-shaders/setup_vxvy-comp.spv");
const SHADER_STEP_POSSION: &[u8] = include_bytes!("../compiled-shaders/step_possion-comp.spv");
const SHADER_V: &[u8] = include_bytes!("../compiled-shaders/v-comp.spv");

type Float = f32;
const WIDTH: usize = 500;
const HEIGHT: usize = 500;
const DISPATCH_SIZE: usize = 50;

const BUFFER_SIZE: u64 = (WIDTH * HEIGHT * std::mem::size_of::<Float>() / std::mem::size_of::<u8>()) as u64;

async fn allocate_scalarfield(device: &Device) -> Result<Buffer, String> {
    let cont = [0 as Float; WIDTH * HEIGHT];

    let byte_buf = bytemuck::cast_slice::<Float, u8>(&cont);

    assert_eq!(byte_buf.len(), BUFFER_SIZE as usize);

    let buf = device.create_buffer_with_data(
        byte_buf,
        wgpu::BufferUsage::MAP_READ | wgpu::BufferUsage::UNIFORM | wgpu::BufferUsage::STORAGE // what is a storage?
    );

    Ok(buf)
}

async fn create_compute_shader(device: &wgpu::Device, spirv: &[u8], bind_group_layouts: &[&wgpu::BindGroupLayout]) -> Result<(wgpu::ShaderModule, wgpu::ComputePipeline), String> {
    let comp_shader_spirv = wgpu::read_spirv(Cursor::new(spirv)).map_err(|e| e.to_string())?;
    let comp_mod = device.create_shader_module(&comp_shader_spirv);

    let comp_layout = device.create_pipeline_layout(
        &wgpu::PipelineLayoutDescriptor {
            bind_group_layouts,
        },
    );

    let compute_pipeline = device.create_compute_pipeline(
        &wgpu::ComputePipelineDescriptor {
            layout: &comp_layout,
            compute_stage: wgpu::ProgrammableStageDescriptor {
                module: &comp_mod,
                entry_point: "main",
            },
        }
    );

    Ok((comp_mod, compute_pipeline))
}

async fn write_image(
    device: &wgpu::Device,
    (r, g, b): (Option<&wgpu::Buffer>, Option<&wgpu::Buffer>, Option<&wgpu::Buffer>),
    name: &str
) -> Result<(), String> {
    let rgba_buffer: &mut [u8] = &mut [255; WIDTH * HEIGHT * 4]; // Default to white

    for (i, col_buf) in vec![r, g, b].into_iter().enumerate() {
        let col_buf = if let Some(col_buf) = col_buf {
            col_buf
        } else {
            continue;
        };

        let mapped_fut = col_buf.map_read(0, BUFFER_SIZE);
        device.poll(wgpu::Maintain::Wait);
        let mapped_mem = mapped_fut.await.map_err(|_| "mapping failed")?;
        let color_data = bytemuck::cast_slice::<u8, f32>(mapped_mem.as_slice());

        for j in 0..WIDTH * HEIGHT {
            let datum_mapped = (color_data[j] + 1.) * 128.;
            rgba_buffer[j * 4 + i] = datum_mapped.min(255.).max(0.) as u8;
        }
    }

    let path = format!("image-dumps/{}.png", name);

    let enc = image::png::PNGEncoder::new(std::fs::File::create(path).map_err(|e| format!("File error: {:?}", e))?);

    enc.write_image(rgba_buffer, WIDTH as u32, HEIGHT as u32, image::ColorType::Rgba8).map_err(|e| format!("Writing image failed: {:?}", e))?;
    Ok(())
}

async fn read_buffer(device: &wgpu::Device, buf: &wgpu::Buffer) -> Result<(), String> {
    let mapped_fut = buf.map_read(0, BUFFER_SIZE);
    device.poll(wgpu::Maintain::Wait);
    let mapped_mem = mapped_fut.await.map_err(|_| "mapping failed")?;
    let conts = bytemuck::cast_slice::<u8, f32>(mapped_mem.as_slice());

    for row in 0..HEIGHT {
        for col in 0..HEIGHT {
            print!("  {:.7}", conts[col + row * WIDTH]);
        }
        println!();
    }
    Ok(())
}

async fn run() -> Result<(), String> {
    let adapter = Adapter::request(
        &wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::Default,
            compatible_surface: None,
        },
        wgpu::BackendBit::PRIMARY,
    ).await.ok_or("No adapter available")?;

    eprintln!("Running on {:?}", adapter.get_info());

    let (device, queue) = adapter.request_device(&wgpu::DeviceDescriptor::default()).await;

    let pressure = allocate_scalarfield(&device).await?;
    let vel_x = allocate_scalarfield(&device).await?;
    let vel_y = allocate_scalarfield(&device).await?;

    let tmp_pressure_poisson_rhs = allocate_scalarfield(&device).await?;

    // Load compute shader


    let bind_group_layout = device.create_bind_group_layout(
        &wgpu::BindGroupLayoutDescriptor {
            label: None,
            bindings: &[
                wgpu::BindGroupLayoutEntry { // Pressure
                    binding: 0,
                    visibility: wgpu::ShaderStage::COMPUTE,
                    ty: wgpu::BindingType::UniformBuffer {
                        dynamic: false,
                    },
                },
                wgpu::BindGroupLayoutEntry { // Temporary pression poisson RHS
                    binding: 1,
                    visibility: wgpu::ShaderStage::COMPUTE,
                    ty: wgpu::BindingType::UniformBuffer {
                        dynamic: false,
                    },
                },
                wgpu::BindGroupLayoutEntry { // vx
                    binding: 2,
                    visibility: wgpu::ShaderStage::COMPUTE,
                    ty: wgpu::BindingType::UniformBuffer {
                        dynamic: false,
                    },
                },
                wgpu::BindGroupLayoutEntry { // vy
                    binding: 3,
                    visibility: wgpu::ShaderStage::COMPUTE,
                    ty: wgpu::BindingType::UniformBuffer {
                        dynamic: false,
                    },
                },
            ],
        },
    );

    let bind_group = device.create_bind_group(
        &wgpu::BindGroupDescriptor {
            label: None,
            layout: &bind_group_layout,
            bindings: &[
                wgpu::Binding {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer {
                        buffer: &pressure,
                        range: 0..BUFFER_SIZE,
                    },
                },
                wgpu::Binding {
                    binding: 1,
                    resource: wgpu::BindingResource::Buffer {
                        buffer: &tmp_pressure_poisson_rhs,
                        range: 0..BUFFER_SIZE,
                    },
                },
                wgpu::Binding {
                    binding: 2,
                    resource: wgpu::BindingResource::Buffer {
                        buffer: &vel_x,
                        range: 0..BUFFER_SIZE,
                    },
                },
                wgpu::Binding {
                    binding: 3,
                    resource: wgpu::BindingResource::Buffer {
                        buffer: &vel_y,
                        range: 0..BUFFER_SIZE,
                    },
                },
            ],
        },
    );

    let (_comp_mod, setup_compute_pipeline) = create_compute_shader(&device, SHADER_SETUP_V, &[&bind_group_layout]).await?;
    let (_comp_mod, possion_compute_pipeline) = create_compute_shader(&device, SHADER_POSSION_RHS, &[&bind_group_layout]).await?;
    let (_comp_mod, step_possion_pipeline) = create_compute_shader(&device, SHADER_STEP_POSSION, &[&bind_group_layout]).await?;
    let (_comp_mod, v_pipeline) = create_compute_shader(&device, SHADER_V, &[&bind_group_layout]).await?;

    // Dispatch!

    let mut encoder = device.create_command_encoder(
        &wgpu::CommandEncoderDescriptor {
            label: None,
        }
    );
    {
        let mut pass = encoder.begin_compute_pass();
        pass.set_pipeline(&setup_compute_pipeline);
        pass.set_bind_group(
            0,
            &bind_group,
            &[],
        );

        pass.dispatch((WIDTH * HEIGHT / DISPATCH_SIZE) as u32, 1, 1);
    }
    let cmd_buf = encoder.finish();

    println!("Submitting");
    queue.submit(&[cmd_buf]);
    println!("Done");

    for frame in 0..375i32 {
        println!("Doing frame {:?}", frame);

        let mut encoder = device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor {
                label: None,
            }
        );


        {
            let mut pass = encoder.begin_compute_pass();
            pass.set_pipeline(&possion_compute_pipeline);
            pass.set_bind_group(
                0,
                &bind_group,
                &[],
            );

            pass.dispatch((WIDTH * HEIGHT / DISPATCH_SIZE) as u32, 1, 1);
        }

        for i in 0..200 {
            let mut pass = encoder.begin_compute_pass();
            pass.set_pipeline(&step_possion_pipeline);
            pass.set_bind_group(
                0,
                &bind_group,
                &[],
            );

            pass.dispatch((WIDTH * HEIGHT / DISPATCH_SIZE) as u32, 1, 1);
        }

        {
            let mut pass = encoder.begin_compute_pass();
            pass.set_pipeline(&v_pipeline);
            pass.set_bind_group(
                0,
                &bind_group,
                &[],
            );

            pass.dispatch((WIDTH * HEIGHT / DISPATCH_SIZE) as u32, 1, 1);

        }

        let cmd_buf = encoder.finish();

        println!("Submitting");
        queue.submit(&[cmd_buf]);
        println!("Done");

        // // Read buffer
        // println!("vx:");
        // read_buffer(&device, &vel_x).await?;
        // println!("vy:");
        // read_buffer(&device, &vel_y).await?;

        // println!("\npossion rhs:");
        // read_buffer(&device, &tmp_pressure_poisson_rhs).await?;

        if frame % 3 == 0 {
            write_image(
                &device,
                (Some(&vel_x), Some(&vel_y), Some(&pressure)),
                &format!("velocity-res-{}", frame),
            ).await?;
        }
    }

    println!("bye");

    Ok(())
}

fn main() {
    if let Err(e) = block_on(run()) {
        eprintln!("died: {:?}", e);
    }
}
