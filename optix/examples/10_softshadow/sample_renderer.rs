use optix::cuda;
use optix::math::*;

type Result<T, E = Error> = std::result::Result<T, E>;

use optix::{
    BufferElement, BufferFormat, DeviceShareable, SbtRecord, SharedVariable,
};
use optix_derive::device_shared;

use std::rc::Rc;
use std::sync::Arc;
use optix::cuda::Allocator;


// enum_from_primitive! {
#[repr(u64)]
#[derive(Debug, PartialEq)]
pub enum MemTags {
    OutputBuffer = 1001,
    SBT = 2001,
    MissRecords = 2002,
    HgRecords = 2003,
    LaunchParams = 3001,
    VertexBuffer = 4001,
    IndexBuffer = 4002,
    NormalBuffer = 4003,
    TexcoordBuffer = 4004,
    Accel = 5001,
}
// }

pub struct SampleRenderer <'a, AllocT>
    where
        AllocT: 'a + Allocator, {
    alloc: &'a AllocT,
    cuda_context: cuda::ContextRef,
    stream: cuda::Stream,
    ctx: optix::DeviceContext,
    pipeline: optix::PipelineRef,
    sbt: optix::ShaderBindingTable<'a, 'a, AllocT>,
    launch_params: SharedVariable<'a, AllocT, LaunchParams<'a, AllocT>>,
    last_set_camera: Camera,
}

impl <'a, AllocT> SampleRenderer<'a, AllocT>
    where
        AllocT: 'a + Allocator, {
    pub fn new(
        fb_size: V2i32,
        camera: Camera,
        model: Model,
        light: QuadLight,
        alloc: &'a AllocT,
    ) -> Result<SampleRenderer<'a, AllocT>> {
        // Make sure CUDA context is initialized
        cuda::init();
        // Check that we've got available devices
        let num_devices = cuda::get_device_count();
        println!("Found {} CUDA devices", num_devices);

        // Initialize optix function table. Must be called before calling
        // any OptiX functions.
        optix::init()?;

        // Just use the first device
        cuda::set_device(0)?;

        // Create a new stream to submit work on
        let stream = cuda::Stream::new()?;

        // Get the device properties
        let device_prop = cuda::get_device_properties(0)?;
        println!(
            "Running on device 0: {} {}MB",
            device_prop.name(),
            device_prop.total_global_mem() / (1024 * 1024)
        );

        // Get the CUDA context for the current thread
        let cuda_context = cuda::Context::get_current()?;

        // Create the device context and enable logging
        let mut ctx = optix::DeviceContext::create(cuda_context, None)?;
        ctx.set_log_callback(
            |level, tag, msg| println!("[{}]: {}", tag, msg),
            4,
        );

        // Create our first module
        let module_compile_options = optix::ModuleCompileOptions {
            max_register_count: 100,
            opt_level: optix::CompileOptimizationLevel::Level0,
            debug_level: optix::CompileDebugLevel::LineInfo,
        };

        let pipeline_compile_options = optix::PipelineCompileOptions {
            uses_motion_blur: false,
            traversable_graph_flags:
                optix::module::TraversableGraphFlags::ALLOW_ANY,
            num_payload_values: 2,
            num_attribute_values: 2,
            exception_flags: optix::module::ExceptionFlags::NONE,
            pipeline_launch_params_variable_name: "optixLaunchParams".into(),
        };

        // Compile our programs to PTX
        // Any struct tagged with the #[device_shared] attribute can be shared
        // with CUDA, and have a C declaration generated for it automatically
        let header = cuda::nvrtc::Header {
            name: "launch_params.h".into(),
            contents: format!(
                "{} {} {} {} {} {}",
                optix::Buffer::<'a, AllocT, i32>::cuda_decl(),
                Frame::<'a, AllocT>::cuda_decl(),
                RenderCamera::cuda_decl(),
                RenderLight::cuda_decl(),
                LaunchParams::<'a, AllocT>::cuda_decl(),
                TriangleMeshSBTData::<'a, AllocT>::cuda_decl(),
            ),
        };
        let cuda_source = include_str!("devicePrograms.cu");
        let ptx = compile_to_ptx(cuda_source, header);

        // Create the module
        let (module, log) = ctx.module_create_from_ptx(
            module_compile_options,
            &pipeline_compile_options,
            &ptx,
        )?;

        if !log.is_empty() {
            println!("{}", log);
        }

        // Create raygen program(s)
        let (raygen_pg, log) = ctx.program_group_create(
            optix::ProgramGroupDesc::Raygen(optix::ProgramGroupModule {
                module: module.clone(),
                entry_function_name: "__raygen__renderFrame".into(),
            }),
        )?;

        // Create miss program(s)
        let mut miss_pgs = Vec::new();
        let (miss_radiance, log) = ctx.program_group_create(
            optix::ProgramGroupDesc::Miss(optix::ProgramGroupModule {
                module: module.clone(),
                entry_function_name: "__miss__radiance".into(),
            }),
        )?;
        miss_pgs.push(miss_radiance);
        let (miss_shadow, log) = ctx.program_group_create(
            optix::ProgramGroupDesc::Miss(optix::ProgramGroupModule {
                module: module.clone(),
                entry_function_name: "__miss__shadow".into(),
            }),
        )?;
        miss_pgs.push(miss_shadow);

        // Create hitgroup programs
        let mut hitgroup_pgs = Vec::new();
        let (hitgroup_radiance, log) =
            ctx.program_group_create(optix::ProgramGroupDesc::Hitgroup {
                ch: Some(optix::ProgramGroupModule {
                    module: module.clone(),
                    entry_function_name: "__closesthit__radiance".into(),
                }),
                ah: Some(optix::ProgramGroupModule {
                    module: module.clone(),
                    entry_function_name: "__anyhit__radiance".into(),
                }),
                is: None,
            })?;
        hitgroup_pgs.push(hitgroup_radiance);
        let (hitgroup_shadow, log) =
            ctx.program_group_create(optix::ProgramGroupDesc::Hitgroup {
                ch: Some(optix::ProgramGroupModule {
                    module: module.clone(),
                    entry_function_name: "__closesthit__shadow".into(),
                }),
                ah: Some(optix::ProgramGroupModule {
                    module: module.clone(),
                    entry_function_name: "__anyhit__shadow".into(),
                }),
                is: None,
            })?;
        hitgroup_pgs.push(hitgroup_shadow);

        // Create the pipeline
        let pipeline_link_options = optix::PipelineLinkOptions {
            max_trace_depth: 2,
            debug_level: optix::CompileDebugLevel::None,
            override_uses_motion_blur: false,
        };

        let mut program_groups = vec![Arc::clone(&raygen_pg)];
        program_groups.extend(miss_pgs.iter().map(|p| Arc::clone(p)));
        program_groups.extend(hitgroup_pgs.iter().map(|p| Arc::clone(p)));
        let (mut pipeline, log) = ctx.pipeline_create(
            &pipeline_compile_options,
            pipeline_link_options,
            &program_groups,
        )?;

        ctx.pipeline_set_stack_size(
            &mut pipeline,
            // direct stack size for direct callables invoked for IS or AH
            2 * 1024,
            // direct stack size for direct callables invoked from RG, MS or CH
            2 * 1024,
            // continuation stack size
            2 * 1024,
            // maximum depth of a traversable graph passed to trace
            3,
        );

        // load textures
        let mut texture_objects = Vec::new();
        for texture in &model.textures {
            let channel_desc = cuda::ChannelFormatDesc {
                x: 8,
                y: 8,
                z: 8,
                w: 8,
                f: cuda::ChannelFormatKind::Unsigned,
            };

            let array = cuda::Array::new(
                &texture.pixels,
                channel_desc,
                texture.resolution.x as usize,
                texture.resolution.y as usize,
                4,
                cuda::ArrayFlags::DEFAULT,
            )
            .unwrap();

            let tex_desc = cuda::TextureDesc::new()
                .address_mode([cuda::TextureAddressMode::Wrap; 3])
                .filter_mode(cuda::TextureFilterMode::Linear)
                .read_mode(cuda::TextureReadMode::NormalizedFloat)
                .normalized_coords(true)
                .srgb(true)
                .build();

            let texture_object = cuda::TextureObject::new(
                cuda::ResourceDesc::Array(array),
                &tex_desc,
            )
            .unwrap();

            texture_objects.push(Rc::new(texture_object));
        }

        // Build raygen and miss SBT records
        let rg_rec = SbtRecord::new(0i32, std::sync::Arc::clone(&raygen_pg));
        let miss_recs = miss_pgs
            .iter()
            .map(|p| SbtRecord::new(0i32, std::sync::Arc::clone(&p)))
            .collect::<Vec<_>>();

        // Loop over all the meshes in the model.
        // For each mesh:
        // 1) Create device primvar and index buffers
        // 2) Create the TriangleMeshSBTData referencing those buffers that we
        //    will use to reference those primvars in the hit programs.
        // 3) Create the hitgroup SBT records with the TriangleMeshSBTData
        // 4) Create the acceleration structure build input
        let mut hg_recs = Vec::new();
        let mut build_inputs = Vec::with_capacity(model.meshes.len());
        for mesh in &model.meshes {
            let vertex_buffer =
                Rc::new(optix::Buffer::new(&mesh.vertex,MemTags::VertexBuffer as u64,
                                           alloc,).unwrap());
            let index_buffer =
                Rc::new(optix::Buffer::new(&mesh.index,
                                           MemTags::IndexBuffer as u64,
                                           alloc,).unwrap());
            let normal_buffer =
                Rc::new(optix::Buffer::new(&mesh.normal,
                                           MemTags::NormalBuffer as u64,
                                           alloc,).unwrap());
            let texcoord_buffer =
                Rc::new(optix::Buffer::new(&mesh.texcoord,
                                           MemTags::TexcoordBuffer as u64,
                                           alloc,).unwrap());

            for pg in &hitgroup_pgs {
                let (has_texture, texture) =
                    if let Some(texture_id) = mesh.diffuse_texture_id {
                        (true, Some(Rc::clone(&texture_objects[texture_id])))
                    } else {
                        (false, None)
                    };

                let mesh_sbt_data = TriangleMeshSBTData {
                    color: mesh.diffuse.into(),
                    vertex: Rc::clone(&vertex_buffer),
                    index: Rc::clone(&index_buffer),
                    normal: Rc::clone(&normal_buffer),
                    texcoord: Rc::clone(&texcoord_buffer),
                    has_texture,
                    texture,
                };

                let hg_rec =
                    SbtRecord::new(mesh_sbt_data, std::sync::Arc::clone(&pg));

                hg_recs.push(hg_rec);
            }

            let build_input = optix::BuildInput::Triangle(
                optix::TriangleArray::new(
                    vec![vertex_buffer],
                    index_buffer,
                    optix::GeometryFlags::NONE,
                )
                .unwrap(),
            );

            build_inputs.push(build_input);
        }

        // Acceleration structure setup
        let accel_build_options = optix::AccelBuildOptions {
            build_flags: optix::BuildFlags::ALLOW_COMPACTION,
            operation: optix::BuildOperation::Build,
            motion_options: optix::MotionOptions {
                num_keys: 1,
                flags: optix::MotionFlags::NONE,
                time_begin: 0.0,
                time_end: 0.0,
            },
        };
        let blas_buffer_sizes = ctx
            .accel_compute_memory_usage(&accel_build_options, &build_inputs)?;
        let compacted_size_buffer =
            cuda::Buffer::new(std::mem::size_of::<usize>(),
                              std::mem::align_of::<usize>(),
                              MemTags::Accel as u64,
                              alloc)?;
        let compacted_size_desc = optix::AccelEmitDesc::new(
            &compacted_size_buffer,
            optix::AccelPropertyType::CompactedSize,
        );

        // Allocate and build acceleration structure
        let temp_buffer =
            cuda::Buffer::new(blas_buffer_sizes[0].temp_size_in_bytes,
                              optix_sys::OptixAccelBufferByteAlignment,
                              MemTags::Accel as u64,
                              alloc)?;
        let output_buffer =
            cuda::Buffer::new(blas_buffer_sizes[0].output_size_in_bytes,
                              optix_sys::OptixAccelBufferByteAlignment,
                              MemTags::Accel as u64,
                              alloc,)?;

        let as_handle = ctx.accel_build(
            &cuda::Stream::default(),
            &accel_build_options,
            &build_inputs,
            &temp_buffer,
            output_buffer,
            std::slice::from_ref(&compacted_size_desc),
        )?;

        // ultimately we'd want to do something more complex with async streams
        // here rather than a stop-the-world sync, but this will do for now.
        cuda::device_synchronize()?;

        // now compact the acceleration structure
        let compacted_size =
            compacted_size_buffer.download_primitive::<usize>()?;

        let as_buffer = cuda::Buffer::new(compacted_size,
                                          optix_sys::OptixAccelBufferByteAlignment,
                                          MemTags::Accel as u64,
                                          alloc,)?;
        let as_handle =
            ctx.accel_compact(&cuda::Stream::default(), as_handle, as_buffer)?;

        // sync again
        cuda::device_synchronize()?;

        // allocate the output buffer
        let color_buffer = optix::Buffer::<'a, AllocT, V4f32>::uninitialized(
            (fb_size.x * fb_size.y) as usize,
            MemTags::OutputBuffer as u64,
            alloc,
        )?;

        // set up the camera
        let cos_fovy = 0.66f32;
        let aspect = fb_size.x as f32 / fb_size.y as f32;
        let direction = normalize(&(camera.at - camera.from));
        let horizontal =
            cos_fovy * aspect * normalize(&cross(&direction, &camera.up));
        let vertical = cos_fovy * normalize(&cross(&horizontal, &direction));

        // Create the LaunchParams struct that will be shared as a __constant__
        // on the device
        let launch_params = LaunchParams {
            frame: Frame {
                color_buffer,
                size: fb_size.into(),
                accum_id: 0,
            },
            camera: RenderCamera {
                position: camera.from.into(),
                direction: direction.into(),
                horizontal: horizontal.into(),
                vertical: vertical.into(),
            },
            light: RenderLight {
                origin: light.origin.into(),
                du: light.du.into(),
                dv: light.dv.into(),
                power: light.power.into(),
            },
            traversable: as_handle,
        };

        // Build the ShaderBindingTable with the records we created earlier
        let sbt = optix::ShaderBindingTable::new(rg_rec,
                                                 MemTags::SBT as u64,
                                                 alloc,)
            .miss_records(miss_recs,MemTags::SBT as u64, alloc)
            .hitgroup_records(hg_recs,MemTags::SBT as u64, alloc)
            .build();

        // Create the SharedVariable that wraps the LaunchParams. This manages
        // the device-side storage for us. When we want to sync to the device
        // we just call upload()
        let launch_params =
            SharedVariable::<'a, AllocT, LaunchParams<'a, AllocT>>::new(launch_params,
                        MemTags::LaunchParams as u64,
                        alloc,)?;

        // Store the pieces we need to persist in the SampleRenderer
        Ok(SampleRenderer {
            alloc,
            cuda_context,
            stream,
            pipeline,
            sbt,
            launch_params,
            last_set_camera: camera,
            ctx,
        })
    }

    pub fn render(&mut self) {
        // increment the frame id on the launch params to seed the RNG
        self.launch_params.frame.accum_id += 1;
        // sync the updated params to the device
        self.launch_params.upload().unwrap();

        // launch the kernel with our pipeline, params and sbt on the given
        // CUDA stream
        self.ctx
            .launch(
                &self.pipeline,
                &self.stream,
                &self.launch_params.variable_buffer(),
                &self.sbt,
                self.launch_params.frame.size.x as u32,
                self.launch_params.frame.size.y as u32,
                1,
            )
            .unwrap();

        // we'll want to do something clever with streams ultimately, but
        // for now do a brute-force sync
        cuda::device_synchronize().unwrap();
    }

    // update the size on the params and allocate a new output buffer
    // params will be synced before next launch in render()
    pub fn resize(&mut self, size: V2i32) {
        self.launch_params.frame.size = size;
        self.launch_params.frame.color_buffer =
            optix::Buffer::<'a, AllocT, V4f32>::uninitialized((size.x * size.y) as usize,
                                                              MemTags::OutputBuffer as u64,
                                                              self.alloc,)
                .unwrap();
    }

    // copy the output image buffer back to the host
    pub fn download_pixels(&self, pixels: &mut [V4f32]) -> Result<()> {
        self.launch_params.frame.color_buffer.download(pixels)?;
        Ok(())
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("OptiX error: {}", _0)]
    OptixError(#[from] optix::Error),
    #[error("CUDA error: {}", _0)]
    CudaError(#[from] cuda::Error),
}

fn compile_to_ptx(src: &str, header: cuda::nvrtc::Header) -> String {
    use cuda::nvrtc::Program;

    let optix_root = std::env::var("OPTIX_ROOT")
        .expect("OPTIX_ROOT not found. You must set OPTIX_ROOT either as an environment variable, or in build-settings.toml to point to the root of your OptiX installation.");

    let cuda_root = std::env::var("CUDA_ROOT")
        .expect("CUDA_ROOT not found. You must set CUDA_ROOT either as an environment variable, or in build-settings.toml to point to the root of your CUDA installation.");

    // Create a vector of options to pass to the compiler
    let optix_inc = format!("-I{}/include", optix_root);
    let cuda_inc = format!("-I{}/include", cuda_root);
    let common_inc = format!(
        "-I{}/examples/common",
        std::env::var("CARGO_MANIFEST_DIR").unwrap()
    );

    let options = vec![
        optix_inc,
        cuda_inc,
        common_inc,
        "-I/usr/include/x86_64-linux-gnu".into(),
        "-I/usr/lib/gcc/x86_64-linux-gnu/7/include".into(),
        "-arch=compute_70".to_owned(),
        "-rdc=true".to_owned(),
        "-std=c++14".to_owned(),
        "-D__x86_64".to_owned(),
        "-D__CUDA_INCLUDE_COMPILER_INTERNAL_HEADERS__=1".into(),
        "-default-device".into(),
    ];

    // The program object allows us to compile the cuda source and get ptx from
    // it if successful.
    let mut prg = Program::new(src, "devicePrograms", &vec![header]).unwrap();

    match prg.compile_program(&options) {
        Err(code) => {
            panic!("{}: {}", code, prg.get_program_log().unwrap());
        }
        Ok(_) => (),
    }

    let ptx = prg.get_ptx().unwrap();
    ptx
}

pub struct Camera {
    pub from: V3f32,
    pub at: V3f32,
    pub up: V3f32,
}

pub struct TriangleMesh {
    pub vertex: Vec<V3f32>,
    pub index: Vec<V3i32>,
    pub color: V3f32,
}

impl TriangleMesh {
    pub fn new(color: V3f32) -> TriangleMesh {
        TriangleMesh {
            color,
            vertex: Vec::new(),
            index: Vec::new(),
        }
    }

    pub fn add_cube(&mut self, center: V3f32, size: V3f32) {
        let start_index = self.vertex.len() as i32;

        self.vertex
            .push((v3f32(0.0, 0.0, 0.0) + center).component_mul(&size));
        self.vertex
            .push((v3f32(1.0, 0.0, 0.0) + center).component_mul(&size));
        self.vertex
            .push((v3f32(0.0, 1.0, 0.0) + center).component_mul(&size));
        self.vertex
            .push((v3f32(1.0, 1.0, 0.0) + center).component_mul(&size));
        self.vertex
            .push((v3f32(0.0, 0.0, 1.0) + center).component_mul(&size));
        self.vertex
            .push((v3f32(1.0, 0.0, 1.0) + center).component_mul(&size));
        self.vertex
            .push((v3f32(0.0, 1.0, 1.0) + center).component_mul(&size));
        self.vertex
            .push((v3f32(1.0, 1.0, 1.0) + center).component_mul(&size));

        const indices: [i32; 36] = [
            0, 1, 3, 2, 3, 0, 5, 7, 6, 5, 6, 4, 0, 4, 5, 0, 5, 1, 2, 3, 7, 2,
            7, 6, 1, 5, 6, 1, 7, 3, 4, 0, 2, 4, 2, 6,
        ];

        for c in indices.chunks(3) {
            self.index.push(v3i32(
                c[0] + start_index,
                c[1] + start_index,
                c[2] + start_index,
            ));
        }
    }
}

#[device_shared]
struct RenderCamera {
    position: V3f32,
    direction: V3f32,
    horizontal: V3f32,
    vertical: V3f32,
}

#[device_shared]
struct Frame<'a, AllocT>
    where
        AllocT: 'a + Allocator, {
    color_buffer: optix::Buffer<'a, AllocT, V4f32>,
    size: V2i32,
    accum_id: i32,
}

#[device_shared]
struct RenderLight {
    origin: V3f32,
    du: V3f32,
    dv: V3f32,
    power: V3f32,
}

#[device_shared]
pub struct LaunchParams<'a, AllocT>
    where
        AllocT: 'a + Allocator, {
    frame: Frame<'a, AllocT>,
    camera: RenderCamera,
    light: RenderLight,
    traversable: optix::TraversableHandle<'a, AllocT>,
}

#[device_shared]
struct TriangleMeshSBTData<'a, AllocT>
    where
        AllocT: 'a + Allocator, {
    color: V3f32,
    vertex: Rc<optix::Buffer<'a, AllocT, V3f32>>,
    normal: Rc<optix::Buffer<'a, AllocT, V3f32>>,
    texcoord: Rc<optix::Buffer<'a, AllocT, V2f32>>,
    index: Rc<optix::Buffer<'a, AllocT, V3i32>>,
    has_texture: bool,
    texture: Option<Rc<cuda::TextureObject>>,
}

pub struct Mesh {
    pub vertex: Vec<V3f32>,
    pub normal: Vec<V3f32>,
    pub texcoord: Vec<V2f32>,
    pub index: Vec<V3i32>,
    pub diffuse: V3f32,
    pub diffuse_texture_id: Option<usize>,
}

pub struct Texture {
    pub pixels: Vec<u8>,
    pub resolution: V2i32,
}

pub struct Model {
    pub meshes: Vec<Mesh>,
    pub textures: Vec<Rc<Texture>>,
    pub bounds: Box3f32,
}

pub struct QuadLight {
    pub origin: V3f32,
    pub du: V3f32,
    pub dv: V3f32,
    pub power: V3f32,
}
