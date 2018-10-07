use glfw::{Action, Context, Key};
use std::ffi::{CStr, CString};
use std::thread;
use crossbeam_channel as channel;
use rand::distributions::{Distribution, Uniform};

pub mod gl_util;
use crate::gl_util::*;

fn main() -> Result<(), String> {
    let mut glfw = glfw::init(glfw::FAIL_ON_ERRORS).unwrap();
    glfw.window_hint(glfw::WindowHint::ContextVersion(4, 1));
    glfw.window_hint(glfw::WindowHint::OpenGlForwardCompat(true));
    glfw.window_hint(glfw::WindowHint::OpenGlProfile(
        glfw::OpenGlProfileHint::Core,
    ));

    let width = 512;
    let height = 512;

    let (mut window, events) = glfw
        .create_window(
            width,
            height,
            "path tracer example",
            glfw::WindowMode::Windowed,
        ).expect("failed to create glfw window");

    window.set_key_polling(true);
    window.make_current();

    // retina displays will return a higher res for the framebuffer
    // which we need to use for the viewport
    let (fb_width, fb_height) = window.get_framebuffer_size();

    let gl = gl::load_with(|s| {
        glfw.get_proc_address_raw(s) as *const std::os::raw::c_void
    });

    let fsq = FullscreenQuad::new(width, height)?;

    let mut image_data = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            image_data.push(f32x4::new(
                (x as f32) / width as f32,
                (y as f32) / height as f32,
                0.0, 1.0
            ));
        }
    }

    unsafe {
        gl::Viewport(0, 0, fb_width, fb_height);
    };

    let (tx_main, rx_main) = channel::unbounded();
    let (tx_render, rx_render) = channel::unbounded();
    thread::spawn(move || {
        let range = Uniform::new(0.0f32, 1.0);
        let mut rng = rand::thread_rng();
        loop {
            match rx_render.try_recv() {
                Some(MsgMaster::StartRender(mut v)) => {
                    let r: f32 = range.sample(&mut rng);
                    let g: f32 = range.sample(&mut rng);
                    let b: f32 = range.sample(&mut rng);

                    for p in v.iter_mut() {
                        p.set(r, g, b, 1.0);
                    }
                    
                    thread::sleep(std::time::Duration::from_millis(1000));
                    
                    tx_main.send(MsgSlave::Progression(v));
                },
                None => (),
            }
        }
    });

    tx_render.send(MsgMaster::StartRender(image_data));

    while !window.should_close() {
        glfw.poll_events();
        for (_, event) in glfw::flush_messages(&events) {
            handle_window_event(&mut window, event);
        }

        match rx_main.try_recv() {
            Some(MsgSlave::Progression(v)) => {
                fsq.update_texture(&v);
                tx_render.send(MsgMaster::StartRender(v));
            },
            None => (),
        }

        // draw the quad
        fsq.draw();

        window.swap_buffers();
    }

    Ok(())
}

enum MsgMaster {
    StartRender(Vec<f32x4>),
}

enum MsgSlave {
    Progression(Vec<f32x4>),
}

fn handle_window_event(window: &mut glfw::Window, event: glfw::WindowEvent) {
    match event {
        glfw::WindowEvent::Key(Key::Escape, _, Action::Press, _) => {
            window.set_should_close(true)
        }
        _ => {}
    }
}
