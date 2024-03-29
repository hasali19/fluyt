#![feature(lint_reasons)]

mod render_thread;
mod task_runner;

use std::ffi::{c_char, c_void, CStr};
use std::sync::{mpsc, Condvar, Mutex};
use std::time::{Duration, Instant};
use std::{mem, ptr};

use color_eyre::Result;
use egl::ClientBuffer;
use flutter_embedder::{
    FlutterCustomTaskRunners, FlutterEngine, FlutterEngineGetCurrentTime,
    FlutterEngineResult_kSuccess, FlutterEngineRun, FlutterEngineRunTask,
    FlutterEngineSendPointerEvent, FlutterEngineSendWindowMetricsEvent,
    FlutterOpenGLRendererConfig, FlutterPointerEvent, FlutterPointerPhase_kAdd,
    FlutterPointerPhase_kDown, FlutterPointerPhase_kHover, FlutterPointerPhase_kRemove,
    FlutterPointerPhase_kUp, FlutterProjectArgs, FlutterRendererConfig,
    FlutterRendererType_kOpenGL, FlutterTask, FlutterTaskRunnerDescription,
    FlutterWindowMetricsEvent, FLUTTER_ENGINE_VERSION,
};
use khronos_egl as egl;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tracing_subscriber::fmt::format::FmtSpan;
use windows::core::{ComInterface, Interface};
use windows::Foundation::Numerics::{Matrix4x4, Vector2, Vector3};
use windows::Foundation::Size;
use windows::Graphics::DirectX::{DirectXAlphaMode, DirectXPixelFormat};
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dwm::{
    DwmFlush, DwmSetWindowAttribute, DWMSBT_TABBEDWINDOW, DWMWA_SYSTEMBACKDROP_TYPE,
    DWM_SYSTEMBACKDROP_TYPE,
};
use windows::Win32::System::WinRT::Composition::{
    ICompositionDrawingSurfaceInterop, ICompositorDesktopInterop, ICompositorInterop,
};
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DispatcherQueueOptions, DQTAT_COM_ASTA, DQTYPE_THREAD_CURRENT,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{DefWindowProcW, WM_NCCALCSIZE};
use windows::UI::Composition::Core::CompositorController;
use windows::UI::Composition::{CompositionDrawingSurface, SpriteVisual};
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use winit::platform::windows::WindowBuilderExtWindows;
use winit::window::{Theme, WindowBuilder};

use crate::render_thread::{RenderEvent, RenderTask};
use crate::task_runner::TaskRunner;

macro_rules! cstr {
    ($v:literal) => {
        concat!($v, "\0").as_ptr() as *const std::ffi::c_char
    };
}

type EglInstance = egl::Instance<egl::Static>;

enum ResizeState {
    Started(u32, u32),
    FrameGenerated,
    Done,
}

struct Gl {
    egl: EglInstance,
    display: egl::Display,
    context: egl::Context,
    resource_context: egl::Context,
    surface: Option<egl::Surface>,
    config: egl::Config,
    compositor_controller: CompositorController,
    visual: SpriteVisual,
    composition_surface: CompositionDrawingSurface,
    resize_condvar: Condvar,
    resize_state: Mutex<ResizeState>,
}

const EGL_PLATFORM_ANGLE_ANGLE: egl::Enum = 0x3202;
const EGL_PLATFORM_ANGLE_TYPE_ANGLE: egl::Attrib = 0x3203;
const EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE: egl::Attrib = 0x3208;

struct WindowData {
    engine: FlutterEngine,
    gl: *mut Gl,
    scale_factor: f64,
}

#[allow(unused)]
extern "C" {
    fn eglDebugMessageControlKHR(
        callback: extern "C" fn(
            egl::Enum,
            *const c_char,
            egl::Int,
            *const c_void,
            *const c_void,
            *const c_char,
        ),
        attribs: *const egl::Attrib,
    ) -> egl::Int;

    fn eglCreateDeviceANGLE(
        device_type: egl::Int,
        native_device: *mut c_void,
        attrib_list: *const egl::Attrib,
    ) -> *mut c_void;

    fn eglReleaseDeviceANGLE(device: *mut c_void);

    fn eglPostSubBufferNV(
        display: *mut c_void,
        surface: *mut c_void,
        x: egl::Int,
        y: egl::Int,
        width: egl::Int,
        height: egl::Int,
    ) -> egl::Boolean;

    fn eglQueryDisplayAttribEXT(
        display: *mut c_void,
        attribute: egl::Int,
        value: *mut egl::Attrib,
    ) -> egl::Boolean;

    fn eglQueryDeviceAttribEXT(
        device: *mut c_void,
        attribute: egl::Int,
        value: *mut egl::Attrib,
    ) -> egl::Boolean;
}

extern "C" fn debug_callback(
    _error: egl::Enum,
    _command: *const c_char,
    _message_type: egl::Int,
    _thread_label: *const c_void,
    _object_label: *const c_void,
    message: *const c_char,
) {
    let message = unsafe { CStr::from_ptr(message) };
    let message = message.to_str().unwrap();
    eprintln!("{message}");
}

#[derive(Debug)]
enum PlatformEvent {
    PostFlutterTask(u64, FlutterTask),
}

fn main() -> Result<()> {
    color_eyre::install()?;

    tracing_subscriber::fmt()
        .with_span_events(FmtSpan::ENTER)
        .with_thread_names(true)
        .init();

    let event_loop = EventLoopBuilder::<PlatformEvent>::with_user_event().build()?;
    let window = WindowBuilder::new()
        .with_inner_size(LogicalSize::new(800, 600))
        .with_no_redirection_bitmap(true)
        .with_theme(Some(Theme::Light))
        .build(&event_loop)?;

    let hwnd = match window.window_handle()?.as_raw() {
        RawWindowHandle::Win32(handle) => HWND(handle.hwnd.get()),
        _ => unreachable!(),
    };

    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &DWMSBT_TABBEDWINDOW as *const DWM_SYSTEMBACKDROP_TYPE as *const c_void,
            mem::size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32,
        )
    }?;

    let PhysicalSize { width, height } = window.inner_size();

    tracing::info!(width, height);

    let _dispatcher_queue_controller = unsafe {
        CreateDispatcherQueueController(DispatcherQueueOptions {
            dwSize: mem::size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_CURRENT,
            apartmentType: DQTAT_COM_ASTA,
        })?
    };

    let compositor_controller = CompositorController::new()?;
    let composition_target = unsafe {
        compositor_controller
            .Compositor()?
            .cast::<ICompositorDesktopInterop>()?
            .CreateDesktopWindowTarget(hwnd, false)?
    };

    let root = compositor_controller.Compositor()?.CreateSpriteVisual()?;

    root.SetSize(Vector2 {
        X: width as f32,
        Y: height as f32,
    })?;

    root.SetTransformMatrix(Matrix4x4 {
        M11: 1.0,
        M22: -1.0,
        M33: 1.0,
        M44: 1.0,
        ..Default::default()
    })?;

    root.SetOffset(Vector3::new(0.0, height as f32, 0.0))?;

    composition_target.SetRoot(&root)?;

    let egl = EglInstance::new(egl::Static);

    let attribs = [egl::NONE as egl::Attrib];
    unsafe { eglDebugMessageControlKHR(debug_callback, attribs.as_ptr()) };

    let display = unsafe {
        egl.get_platform_display(
            EGL_PLATFORM_ANGLE_ANGLE,
            egl::DEFAULT_DISPLAY,
            &[
                EGL_PLATFORM_ANGLE_TYPE_ANGLE,
                EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE,
                egl::NONE as egl::Attrib,
            ],
        )
    }?;

    egl.initialize(display)?;

    let device = unsafe {
        let mut egl_device = 0;
        assert!(
            eglQueryDisplayAttribEXT(
                display.as_ptr(),
                0x322C, /* EGL_DEVICE_EXT */
                &mut egl_device,
            ) == egl::TRUE
        );
        let mut angle_device = 0;
        assert!(
            eglQueryDeviceAttribEXT(
                egl_device as _,
                0x33A1, /* EGL_D3D11_DEVICE_ANGLE */
                &mut angle_device
            ) == egl::TRUE
        );
        ID3D11Device::from_raw(angle_device as _)
    };

    let composition_device = unsafe {
        compositor_controller
            .Compositor()?
            .cast::<ICompositorInterop>()?
            .CreateGraphicsDevice(&device)?
    };

    let composition_surface = composition_device.CreateDrawingSurface(
        Size {
            Width: width as f32,
            Height: height as f32,
        },
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        DirectXAlphaMode::Premultiplied,
    )?;

    root.SetBrush(
        &compositor_controller
            .Compositor()?
            .CreateSurfaceBrushWithSurface(&composition_surface)?,
    )?;

    let mut configs = Vec::with_capacity(1);
    let config_attribs = [
        egl::RED_SIZE,
        8,
        egl::GREEN_SIZE,
        8,
        egl::BLUE_SIZE,
        8,
        egl::ALPHA_SIZE,
        8,
        egl::DEPTH_SIZE,
        8,
        egl::STENCIL_SIZE,
        8,
        egl::NONE,
    ];

    egl.choose_config(display, &config_attribs, &mut configs)?;

    let context_attribs = [egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE];
    let context = egl.create_context(display, configs[0], None, &context_attribs)?;
    let resource_context =
        egl.create_context(display, configs[0], Some(context), &context_attribs)?;

    egl.make_current(display, None, None, Some(context))?;

    gl::Flush::load_with(|name| egl.get_proc_address(name).unwrap() as _);

    let gl = Box::leak(Box::new(Gl {
        egl,
        display,
        context,
        resource_context,
        surface: None,
        config: configs[0],
        compositor_controller,
        visual: root,
        composition_surface,
        resize_condvar: Condvar::new(),
        resize_state: Mutex::new(ResizeState::Done),
    }));

    let engine = unsafe { create_engine(gl, event_loop.create_proxy()) };

    unsafe {
        FlutterEngineSendWindowMetricsEvent(
            engine,
            &FlutterWindowMetricsEvent {
                struct_size: mem::size_of::<FlutterWindowMetricsEvent>(),
                width: width as usize,
                height: height as usize,
                pixel_ratio: window.scale_factor(),
                ..Default::default()
            },
        )
    };

    gl.egl.make_current(display, None, None, None)?;

    assert!(gl.egl.get_current_context().is_none());
    assert!(gl.egl.get_current_display().is_none());

    let window_data = Box::leak(Box::new(WindowData {
        engine,
        gl,
        scale_factor: window.scale_factor(),
    }));

    unsafe { SetWindowSubclass(hwnd, Some(wnd_proc), 696969, window_data as *mut _ as _) };

    let mut cursor_pos = PhysicalPosition::new(0.0, 0.0);
    let mut tasks = vec![];

    event_loop.run(move |event, target| {
        match event {
            Event::UserEvent(event) => match event {
                PlatformEvent::PostFlutterTask(target_time_nanos, task) => {
                    tasks.push((target_time_nanos, task));
                }
            },
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    target.exit();
                }
                WindowEvent::ScaleFactorChanged {
                    scale_factor,
                    inner_size_writer: _,
                } => {
                    window_data.scale_factor = scale_factor;
                }
                WindowEvent::CursorMoved { position, .. } => unsafe {
                    cursor_pos = position;
                    FlutterEngineSendPointerEvent(
                        engine,
                        &FlutterPointerEvent {
                            struct_size: mem::size_of::<FlutterPointerEvent>(),
                            phase: FlutterPointerPhase_kHover,
                            x: position.x,
                            y: position.y,
                            timestamp: FlutterEngineGetCurrentTime() as usize,
                            ..Default::default()
                        },
                        1,
                    );
                },
                WindowEvent::CursorEntered { .. } => unsafe {
                    FlutterEngineSendPointerEvent(
                        engine,
                        &FlutterPointerEvent {
                            struct_size: mem::size_of::<FlutterPointerEvent>(),
                            phase: FlutterPointerPhase_kAdd,
                            x: cursor_pos.x,
                            y: cursor_pos.y,
                            timestamp: FlutterEngineGetCurrentTime() as usize,
                            ..Default::default()
                        },
                        1,
                    );
                },
                WindowEvent::CursorLeft { .. } => unsafe {
                    FlutterEngineSendPointerEvent(
                        engine,
                        &FlutterPointerEvent {
                            struct_size: mem::size_of::<FlutterPointerEvent>(),
                            phase: FlutterPointerPhase_kRemove,
                            x: cursor_pos.x,
                            y: cursor_pos.y,
                            timestamp: FlutterEngineGetCurrentTime() as usize,
                            ..Default::default()
                        },
                        1,
                    );
                },
                WindowEvent::MouseInput { state, .. } => unsafe {
                    FlutterEngineSendPointerEvent(
                        engine,
                        &FlutterPointerEvent {
                            struct_size: mem::size_of::<FlutterPointerEvent>(),
                            phase: match state {
                                ElementState::Pressed => FlutterPointerPhase_kDown,
                                ElementState::Released => FlutterPointerPhase_kUp,
                            },
                            x: cursor_pos.x,
                            y: cursor_pos.y,
                            timestamp: FlutterEngineGetCurrentTime() as usize,
                            ..Default::default()
                        },
                        1,
                    );
                },
                _ => {}
            },
            _ => (),
        }

        let now = unsafe { FlutterEngineGetCurrentTime() };
        let mut next_task_target_time = None;

        tasks.retain(|(target_time_nanos, task)| {
            if now >= *target_time_nanos {
                unsafe { FlutterEngineRunTask(engine, task) };
                return false;
            }

            let delta = Duration::from_nanos(target_time_nanos - now);
            let target_time = Instant::now() + delta;

            next_task_target_time = Some(if let Some(next) = next_task_target_time {
                std::cmp::min(next, target_time)
            } else {
                target_time
            });

            true
        });

        if let Some(next) = next_task_target_time {
            target.set_control_flow(ControlFlow::WaitUntil(next));
        }
    })?;

    Ok(())
}

unsafe extern "system" fn wnd_proc(
    window: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _uidsubclass: usize,
    dwrefdata: usize,
) -> LRESULT {
    let data = dwrefdata as *mut WindowData;
    match msg {
        WM_NCCALCSIZE => {
            DefWindowProcW(window, msg, wparam, lparam);

            let rect = lparam.0 as *const RECT;
            let rect = rect.as_ref().unwrap();

            if !data.is_null() && rect.right > rect.left && rect.bottom > rect.top {
                let mut resize_state = (*(*data).gl).resize_state.lock().unwrap();

                *resize_state = ResizeState::Started(
                    (rect.right - rect.left) as u32,
                    (rect.bottom - rect.top) as u32,
                );

                FlutterEngineSendWindowMetricsEvent(
                    (*data).engine,
                    &FlutterWindowMetricsEvent {
                        struct_size: mem::size_of::<FlutterWindowMetricsEvent>(),
                        width: (rect.right - rect.left) as usize,
                        height: (rect.bottom - rect.top) as usize,
                        pixel_ratio: (*data).scale_factor,
                        ..Default::default()
                    },
                );

                let _unused = (*(*data).gl)
                    .resize_condvar
                    .wait_while(resize_state, |resize_state| {
                        !matches!(resize_state, ResizeState::Done)
                    })
                    .unwrap();
            }
        }
        _ => return DefSubclassProc(window, msg, wparam, lparam),
    }

    LRESULT(0)
}

unsafe fn create_engine(gl: &mut Gl, event_loop: EventLoopProxy<PlatformEvent>) -> FlutterEngine {
    let mut engine = ptr::null_mut();

    fn create_task_runner<F: Fn(u64, FlutterTask)>(
        id: usize,
        runner: &'static TaskRunner<F>,
    ) -> FlutterTaskRunnerDescription {
        unsafe extern "C" fn runs_tasks_on_current_thread<F>(task_runner: *mut c_void) -> bool {
            task_runner
                .cast::<TaskRunner<F>>()
                .as_mut()
                .unwrap()
                .runs_tasks_on_current_thread()
        }

        unsafe extern "C" fn post_task_callback<F: Fn(u64, FlutterTask)>(
            task: FlutterTask,
            target_time_nanos: u64,
            user_data: *mut c_void,
        ) {
            user_data
                .cast::<TaskRunner<F>>()
                .as_mut()
                .unwrap()
                .post_task(task, target_time_nanos)
        }

        FlutterTaskRunnerDescription {
            struct_size: mem::size_of::<FlutterTaskRunnerDescription>(),
            identifier: id,
            user_data: runner as *const TaskRunner<F> as *mut c_void,
            runs_task_on_current_thread_callback: Some(runs_tasks_on_current_thread::<F>),
            post_task_callback: Some(post_task_callback::<F>),
        }
    }

    let (render_tx, render_rx) = mpsc::channel();

    let renderer_config = FlutterRendererConfig {
        type_: FlutterRendererType_kOpenGL,
        __bindgen_anon_1: flutter_embedder::FlutterRendererConfig__bindgen_ty_1 {
            open_gl: FlutterOpenGLRendererConfig {
                struct_size: mem::size_of::<FlutterOpenGLRendererConfig>(),
                make_current: Some(gl_make_current),
                make_resource_current: Some(gl_make_resource_current),
                clear_current: Some(gl_clear_current),
                present: Some(gl_present),
                fbo_callback: Some(gl_fbo_callback),
                fbo_reset_after_present: true,
                gl_proc_resolver: Some(gl_get_proc_address),
                ..Default::default()
            },
        },
    };

    let platform_task_runner = create_task_runner(
        1,
        Box::leak(Box::new(TaskRunner::new(move |t, task| {
            event_loop
                .send_event(PlatformEvent::PostFlutterTask(t, task))
                .unwrap()
        }))),
    );

    let render_task_runner = create_task_runner(
        2,
        Box::leak(Box::new(TaskRunner::new(move |t, task| {
            render_tx
                .send(RenderEvent::PostTask(RenderTask(t, task)))
                .unwrap();
        }))),
    );

    let project_args = FlutterProjectArgs {
        struct_size: mem::size_of::<FlutterProjectArgs>(),
        assets_path: cstr!("example/build/flutter_assets"),
        icu_data_path: cstr!("icudtl.dat"),
        custom_task_runners: &FlutterCustomTaskRunners {
            struct_size: mem::size_of::<FlutterCustomTaskRunners>(),
            platform_task_runner: &platform_task_runner,
            render_task_runner: &render_task_runner,
            thread_priority_setter: Some(task_runner::set_thread_priority),
        },
        ..Default::default()
    };

    unsafe {
        let result = FlutterEngineRun(
            FLUTTER_ENGINE_VERSION as usize,
            &renderer_config,
            &project_args,
            gl as *mut Gl as *mut c_void,
            &mut engine,
        );

        if result != FlutterEngineResult_kSuccess || engine.is_null() {
            panic!("could not run the flutter engine");
        }
    }

    render_thread::start(engine, render_rx);

    engine
}

#[tracing::instrument]
unsafe extern "C" fn gl_make_current(user_data: *mut c_void) -> bool {
    let gl = user_data.cast::<Gl>().as_mut().unwrap();

    let res = gl
        .egl
        .make_current(gl.display, None, None, Some(gl.context));

    if let Err(e) = res {
        eprintln!("failed to make context current: {e}");
    }

    res.is_ok()
}

#[tracing::instrument]
unsafe extern "C" fn gl_make_resource_current(user_data: *mut c_void) -> bool {
    let gl = user_data.cast::<Gl>().as_mut().unwrap();

    let res = gl
        .egl
        .make_current(gl.display, None, None, Some(gl.resource_context));

    if let Err(e) = res {
        eprintln!("failed to make resource context current: {e}");
    }

    res.is_ok()
}

#[tracing::instrument]
unsafe extern "C" fn gl_clear_current(user_data: *mut c_void) -> bool {
    let gl = user_data.cast::<Gl>().as_mut().unwrap();

    let res = gl.egl.make_current(gl.display, None, None, None);

    if let Err(e) = res {
        eprintln!("failed to clear context: {e}");
    }

    res.is_ok()
}

#[tracing::instrument]
unsafe extern "C" fn gl_present(user_data: *mut c_void) -> bool {
    let gl = user_data.cast::<Gl>().as_mut().unwrap();
    let mut resize_state = gl.resize_state.lock().unwrap();

    match *resize_state {
        ResizeState::Started(_, _) => return false,
        ResizeState::FrameGenerated => {
            present_frame(gl, true).unwrap();
            *resize_state = ResizeState::Done;
            gl.resize_condvar.notify_all();
        }
        ResizeState::Done => {
            present_frame(gl, false).unwrap();
        }
    }

    gl.surface = None;

    true
}

unsafe fn present_frame(gl: &Gl, sync_dwm: bool) -> Result<()> {
    let Some(egl_surface) = gl.surface else {
        panic!("BeginDraw() has not been called for composition surface");
    };

    gl::Flush();

    gl.egl.destroy_surface(gl.display, egl_surface)?;
    gl.egl
        .make_current(gl.display, None, None, Some(gl.context))?;

    let composition_surface_interop = gl
        .composition_surface
        .cast::<ICompositionDrawingSurfaceInterop>()?;

    composition_surface_interop.EndDraw()?;

    if sync_dwm {
        DwmFlush()?;
    }

    gl.compositor_controller.Commit()?;

    Ok(())
}

#[tracing::instrument]
unsafe extern "C" fn gl_fbo_callback(user_data: *mut c_void) -> u32 {
    let gl = user_data.cast::<Gl>().as_mut().unwrap();
    let mut resize_state = gl.resize_state.lock().unwrap();

    let composition_surface_interop = gl
        .composition_surface
        .cast::<ICompositionDrawingSurfaceInterop>()
        .unwrap();

    if let ResizeState::Started(width, height) = *resize_state {
        gl.visual
            .SetSize(Vector2 {
                X: width as f32,
                Y: height as f32,
            })
            .unwrap();

        gl.visual
            .SetOffset(Vector3::new(0.0, height as f32, 0.0))
            .unwrap();

        gl.composition_surface
            .Resize(SizeInt32 {
                Width: width as i32,
                Height: height as i32,
            })
            .unwrap();

        *resize_state = ResizeState::FrameGenerated;
    }

    let mut update_offset = POINT::default();
    let texture: ID3D11Texture2D = composition_surface_interop
        .BeginDraw(None, &mut update_offset)
        .unwrap();

    let client_buffer = unsafe { ClientBuffer::from_ptr(texture.as_raw()) };

    let surface = gl
        .egl
        .create_pbuffer_from_client_buffer(
            gl.display,
            0x33A3,
            client_buffer,
            gl.config,
            &[0x3490, update_offset.x, 0x3491, update_offset.y, egl::NONE],
        )
        .unwrap();

    gl.surface = Some(surface);

    gl.egl
        .make_current(gl.display, gl.surface, gl.surface, Some(gl.context))
        .unwrap();

    0
}

unsafe extern "C" fn gl_get_proc_address(
    user_data: *mut c_void,
    name: *const c_char,
) -> *mut c_void {
    let gl = user_data.cast::<Gl>().as_mut().unwrap();
    let name = CStr::from_ptr(name);
    gl.egl.get_proc_address(name.to_str().unwrap()).unwrap() as _
}
