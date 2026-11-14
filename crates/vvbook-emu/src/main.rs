#![forbid(unsafe_code)]

use softbuffer::{Context, Surface};
use std::env;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use vvbook_emu::{
    EmuDisplaySink, EmuPower, EmuSensors, FsStorage, HostEntropy, LivePortalServer,
    export_all_snapshots, seed_sample_library,
};
use vvbook_pal::{Button, InputEvent, PORTRAIT_H, PORTRAIT_W};
use vvbook_userspace::FolioApp;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

struct EmuRuntime {
    app: FolioApp,
    storage: FsStorage,
    sink: EmuDisplaySink,
    sensors: EmuSensors,
    power: EmuPower,
    portal: LivePortalServer,
    entropy: HostEntropy,
}

impl EmuRuntime {
    fn boot_at(
        sdcard_dir: &Path,
        sample_dir: &Path,
        vvdrop_bind: &str,
        allow_ephemeral_fallback: bool,
    ) -> Result<Self, String> {
        let _ = seed_sample_library(sample_dir);
        let mut storage = FsStorage::new(sdcard_dir).map_err(|e| format!("sdcard: {e}"))?;
        storage
            .seed_books_if_empty(sample_dir)
            .map_err(|e| format!("seed books: {e}"))?;
        let mut sink = EmuDisplaySink::new();
        let mut sensors = EmuSensors::default();
        let mut power = EmuPower::default();
        let mut portal = match LivePortalServer::bind(vvdrop_bind, [0u8; 32], 1) {
            Ok(p) => p,
            Err(_) if allow_ephemeral_fallback => LivePortalServer::bind("127.0.0.1:0", [0u8; 32], 1)
                .map_err(|e| format!("portal bind fallback: {e}"))?,
            Err(e) => return Err(format!("portal bind {vvdrop_bind}: {e}")),
        };
        let mut entropy = HostEntropy::from_system();
        let mut app = FolioApp::new();
        app.boot(
            &mut sink,
            &mut storage,
            &mut sensors,
            &mut power,
            &mut portal,
            &mut entropy,
        );
        Ok(Self {
            app,
            storage,
            sink,
            sensors,
            power,
            portal,
            entropy,
        })
    }

    fn press(&mut self, btn: Button) {
        self.app.handle_event(
            InputEvent::Press(btn),
            &mut self.sink,
            &mut self.storage,
            &mut self.sensors,
            &mut self.power,
            &mut self.portal,
            &mut self.entropy,
        );
    }

    fn tick(&mut self) {
        self.app.tick(
            &mut self.sink,
            &mut self.storage,
            &mut self.sensors,
            &mut self.power,
            &mut self.portal,
        );
    }
}

fn run_web_emulator(rt: EmuRuntime, web_bind: &str) -> Result<(), String> {
    let listener = TcpListener::bind(web_bind).map_err(|e| format!("bind {web_bind}: {e}"))?;
    let vv_url = rt.portal.base_url();
    println!(
        "VVBook Interactive Web Emulator listening on http://{web_bind} (VVDrop at {vv_url})"
    );
    let shared = Arc::new(Mutex::new(rt));
    let bg = Arc::clone(&shared);
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(120));
        if let Ok(mut g) = bg.lock() {
            g.tick();
        }
    });

    for stream in listener.incoming().flatten() {
        let mut s = stream;
        let mut buf = [0u8; 2048];
        let Ok(n) = s.read(&mut buf) else { continue };
        let req = String::from_utf8_lossy(&buf[..n]);
        let first = req.lines().next().unwrap_or("");
        let mut parts = first.split_whitespace();
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("/");

        if method == "GET" && path == "/" {
            let html = format!(
                r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>VVBook Xteink X3 Emulator</title>
<style>
body{{background:#E6E2D8;color:#161615;font-family:Inter,'Helvetica Neue',Arial,sans-serif;display:flex;flex-direction:column;align-items:center;padding:28px;margin:0}}
.x3-wrap{{position:relative;display:inline-flex;align-items:flex-start}}
.bezel{{background:#F7F5F0;padding:22px 24px 20px;border-radius:0;box-shadow:8px 8px 0 #161615;border:3px solid #161615}}
img{{display:block;width:396px;height:594px;border-radius:0;border:2px solid #161615;background:#FFF}}
.chin{{display:grid;grid-template-columns:repeat(4,1fr);gap:8px;margin-top:16px;padding:0 18px}}
.chin button{{background:#F7F5F0;color:#161615;border:2px solid #161615;height:40px;border-radius:0;cursor:pointer;font-size:12px;font-weight:700;letter-spacing:.05em;text-transform:uppercase}}
.chin button.ok{{background:#161615;color:#F7F5F0;border-color:#161615}}
.chin button:hover,.side button:hover{{background:#D8D4CA;color:#161615}}
.chin button.ok:hover{{background:#333330;color:#F7F5F0}}
.chin button:active,.side button:active{{transform:translate(2px,2px)}}
.chin-label{{text-align:center;color:#55544E;font-size:11px;margin-top:10px;letter-spacing:.10em;text-transform:uppercase;font-weight:600}}
.side-left{{display:flex;flex-direction:column;gap:8px;margin-top:138px;margin-right:-3px;z-index:1}}
.side{{display:flex;flex-direction:column;gap:8px;margin-top:78px;margin-left:-3px}}
.side button,.side-left button{{background:#F7F5F0;color:#161615;border:2px solid #161615;width:48px;border-radius:0;cursor:pointer;font-size:12px;font-weight:700;display:flex;align-items:center;justify-content:center;box-shadow:4px 4px 0 #161615}}
.side button.pwr{{height:44px;margin-bottom:16px;background:#C83228;color:#F7F5F0;border-color:#161615;font-size:11px;letter-spacing:.06em}}
.side button.pwr:hover{{background:#A8241B;color:#F7F5F0}}
.side button.rocker,.side-left button.rocker{{height:56px}}
</style></head>
<body>
<div class="x3-wrap">
  <div class="side-left">
    <button class="rocker" title="Side Up (Up Arrow)" onclick="send('up')">▲</button>
  </div>
  <div class="bezel">
    <img id="scr" src="/frame.png" alt="X3 Screen"/>
    <div class="chin">
      <button title="Esc / Backspace" onclick="send('back')">Back</button>
      <button class="ok" title="Enter / Space" onclick="send('confirm')">Confirm</button>
      <button title="Left Arrow" onclick="send('left')">◀</button>
      <button title="Right Arrow" onclick="send('right')">▶</button>
    </div>
    <div class="chin-label">Front Chin · Back · Confirm · ◀ · ▶</div>
  </div>
  <div class="side">
    <button class="pwr" title="Power (P)" onclick="send('power')">PWR</button>
    <button class="rocker" title="Side Down (Down Arrow)" onclick="send('down')">▼</button>
  </div>
</div>
<p style="color:#55544E;font-size:12px;margin-top:16px">VVDrop live URL: <code>{vv_url}</code> · Chin: Back · Confirm · ◀ · ▶ | Shoulders: Left ▲ · Right ▼ / PWR | Keys: Arrows, Enter, Esc, P</p>
<script>
async function send(b){{await fetch('/btn/'+b,{{method:'POST'}});refresh();}}
function refresh(){{document.getElementById('scr').src='/frame.png?t='+Date.now();}}
setInterval(refresh,600);
window.addEventListener('keydown',e=>{{
  const m={{ArrowLeft:'left',ArrowRight:'right',ArrowUp:'up',ArrowDown:'down',Enter:'confirm',' ':'confirm',Escape:'back',Backspace:'back',p:'power',P:'power'}};
  if(m[e.key]){{e.preventDefault();send(m[e.key]);}}
}});
</script></body></html>"#
            );
            let hdr = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                html.len()
            );
            let _ = s.write_all(hdr.as_bytes());
            let _ = s.write_all(html.as_bytes());
        } else if method == "GET" && path.starts_with("/frame.png") {
            let png = shared.lock().unwrap().sink.encode_png();
            let hdr = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                png.len()
            );
            let _ = s.write_all(hdr.as_bytes());
            let _ = s.write_all(&png);
        } else if method == "POST" && path.starts_with("/btn/") {
            let btn = match &path[5..] {
                "up" => Some(Button::Up),
                "down" => Some(Button::Down),
                "left" => Some(Button::Left),
                "right" => Some(Button::Right),
                "confirm" => Some(Button::Confirm),
                "back" => Some(Button::Back),
                "power" => Some(Button::Power),
                _ => None,
            };
            if let Some(b) = btn {
                shared.lock().unwrap().press(b);
            }
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK");
        } else {
            let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    }
    Ok(())
}

const EMU_PAD_LEFT: usize = 76;
const EMU_PAD_TOP: usize = 24;
const EMU_PAD_RIGHT: usize = 76;
const EMU_PAD_BOTTOM: usize = 92;
const EMU_WIN_W: usize = PORTRAIT_W + EMU_PAD_LEFT + EMU_PAD_RIGHT;
const EMU_WIN_H: usize = PORTRAIT_H + EMU_PAD_TOP + EMU_PAD_BOTTOM;

struct BezelButton {
    btn: Button,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    label: &'static str,
    accent: bool,
}

fn desktop_bezel_buttons() -> [BezelButton; 7] {
    let chin_y = EMU_PAD_TOP + PORTRAIT_H + 22;
    let chin_w = vvbook_gfx::X3_CHIN_W as usize;
    let chin_h = 44usize;
    let left_x = 12usize;
    let right_x = EMU_PAD_LEFT + PORTRAIT_W + 12;
    [
        BezelButton {
            btn: Button::Back,
            x: EMU_PAD_LEFT + vvbook_gfx::X3_CHIN_X[0] as usize,
            y: chin_y,
            w: chin_w,
            h: chin_h,
            label: "BACK",
            accent: false,
        },
        BezelButton {
            btn: Button::Confirm,
            x: EMU_PAD_LEFT + vvbook_gfx::X3_CHIN_X[1] as usize,
            y: chin_y,
            w: chin_w,
            h: chin_h,
            label: "OK",
            accent: true,
        },
        BezelButton {
            btn: Button::Left,
            x: EMU_PAD_LEFT + vvbook_gfx::X3_CHIN_X[2] as usize,
            y: chin_y,
            w: chin_w,
            h: chin_h,
            label: "<",
            accent: false,
        },
        BezelButton {
            btn: Button::Right,
            x: EMU_PAD_LEFT + vvbook_gfx::X3_CHIN_X[3] as usize,
            y: chin_y,
            w: chin_w,
            h: chin_h,
            label: ">",
            accent: false,
        },
        BezelButton {
            btn: Button::Power,
            x: right_x,
            y: EMU_PAD_TOP + 96,
            w: 52,
            h: 44,
            label: "PWR",
            accent: true,
        },
        BezelButton {
            btn: Button::Up,
            x: left_x,
            y: EMU_PAD_TOP + 168,
            w: 52,
            h: 58,
            label: "^",
            accent: false,
        },
        BezelButton {
            btn: Button::Down,
            x: right_x,
            y: EMU_PAD_TOP + 168,
            w: 52,
            h: 58,
            label: "v",
            accent: false,
        },
    ]
}

fn glyph_5x7(ch: char) -> [u8; 7] {
    match ch {
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'C' => [0b01111, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b01111],
        'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        '<' => [0b00010, 0b00100, 0b01000, 0b10000, 0b01000, 0b00100, 0b00010],
        '>' => [0b01000, 0b00100, 0b00010, 0b00001, 0b00010, 0b00100, 0b01000],
        '^' => [0b00100, 0b01010, 0b10001, 0b00000, 0b00000, 0b00000, 0b00000],
        'v' => [0b00000, 0b00000, 0b00000, 0b00000, 0b10001, 0b01010, 0b00100],
        _ => [0; 7],
    }
}

fn draw_bezel_button(buf: &mut [u32], stride: usize, rows: usize, b: &BezelButton) {
    let fill = if b.accent { 0x003A_3833 } else { 0x002F_2F2B };
    let border = if b.accent { 0x0078_756C } else { 0x0059_5751 };
    for dy in 0..b.h {
        let py = b.y + dy;
        if py >= rows {
            break;
        }
        for dx in 0..b.w {
            let px = b.x + dx;
            if px >= stride {
                break;
            }
            let is_edge = dy == 0 || dy + 1 == b.h || dx == 0 || dx + 1 == b.w;
            buf[py * stride + px] = if is_edge { border } else { fill };
        }
    }
    let text_w = b.label.len() * 12;
    let tx0 = b.x + b.w.saturating_sub(text_w) / 2;
    let ty0 = b.y + b.h.saturating_sub(14) / 2;
    for (ci, ch) in b.label.chars().enumerate() {
        let g = glyph_5x7(ch);
        for (gr, row_bits) in g.iter().enumerate() {
            for gc in 0..5usize {
                if (row_bits >> (4 - gc)) & 1 != 0 {
                    for sy in 0..2 {
                        for sx in 0..2 {
                            let px = tx0 + ci * 12 + gc * 2 + sx;
                            let py = ty0 + gr * 2 + sy;
                            if px < stride && py < rows {
                                buf[py * stride + px] = 0x00F6_F4EE;
                            }
                        }
                    }
                }
            }
        }
    }
}

struct DesktopApp {
    rt: EmuRuntime,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    cursor: (f64, f64),
}

impl DesktopApp {
    fn draw_bezel(&mut self) {
        let Some(ref win) = self.window else { return };
        let Some(ref mut surf) = self.surface else { return };
        let sz = win.inner_size();
        if sz.width == 0 || sz.height == 0 {
            return;
        }
        let _ = surf.resize(
            NonZeroU32::new(sz.width).unwrap(),
            NonZeroU32::new(sz.height).unwrap(),
        );
        if let Ok(mut buf) = surf.buffer_mut() {
            // Anodized matte graphite Xteink X3 bezel surround
            buf.fill(0x0024_2422);
            let frame = self.rt.sink.to_argb_u32();
            let w = sz.width as usize;
            let h = sz.height as usize;
            for y in 0..PORTRAIT_H {
                let dy = EMU_PAD_TOP + y;
                if dy >= h {
                    break;
                }
                for x in 0..PORTRAIT_W {
                    let dx = EMU_PAD_LEFT + x;
                    if dx < w {
                        buf[dy * w + dx] = frame[y * PORTRAIT_W + x];
                    }
                }
            }
            for b in desktop_bezel_buttons() {
                draw_bezel_button(&mut buf, w, h, &b);
            }
            let _ = buf.present();
        }
    }
}

impl ApplicationHandler for DesktopApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let attrs = Window::default_attributes()
                .with_title("VVBook Xteink X3 Emulator · Folio OS")
                .with_inner_size(LogicalSize::new(EMU_WIN_W as u32, EMU_WIN_H as u32));
            let win = Rc::new(event_loop.create_window(attrs).expect("create window"));
            let ctx = Context::new(Rc::clone(&win)).expect("softbuffer context");
            let surf = Surface::new(&ctx, Rc::clone(&win)).expect("softbuffer surface");
            self.window = Some(win);
            self.surface = Some(surf);
            self.draw_bezel();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.draw_bezel(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                let mx = self.cursor.0.max(0.0) as usize;
                let my = self.cursor.1.max(0.0) as usize;
                if let Some(hit) = desktop_bezel_buttons()
                    .into_iter()
                    .find(|b| mx >= b.x && mx < b.x + b.w && my >= b.y && my < b.y + b.h)
                {
                    self.rt.press(hit.btn);
                    self.draw_bezel();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    let btn = match code {
                        KeyCode::ArrowLeft => Some(Button::Left),
                        KeyCode::ArrowRight => Some(Button::Right),
                        KeyCode::ArrowUp => Some(Button::Up),
                        KeyCode::ArrowDown => Some(Button::Down),
                        KeyCode::Enter | KeyCode::Space => Some(Button::Confirm),
                        KeyCode::Backspace | KeyCode::Escape => Some(Button::Back),
                        KeyCode::KeyP => Some(Button::Power),
                        _ => None,
                    };
                    if let Some(b) = btn {
                        self.rt.press(b);
                        self.draw_bezel();
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        let before = self.rt.sink.commits;
        self.rt.tick();
        if self.rt.sink.commits != before {
            self.draw_bezel();
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let sample_dir = PathBuf::from("./sample_library");

    if let Some(pos) = args.iter().position(|a| a == "--snapshot-all") {
        let out_dir = args
            .get(pos + 1)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("./screenshots"));
        match export_all_snapshots(&out_dir, &sample_dir) {
            Ok(files) => {
                for f in &files {
                    println!("Wrote {}", f.display());
                }
                println!("Exported {} screenshots cleanly.", files.len());
                return;
            }
            Err(e) => {
                eprintln!("Snapshot export failed: {e}");
                process::exit(1);
            }
        }
    }

    if let Some(pos) = args.iter().position(|a| a == "--x3-stress") {
        let opt_dir = args
            .get(pos + 1)
            .filter(|v| !v.starts_with("--"))
            .map(PathBuf::from);
        match vvbook_emu::x3_twin::run_x3_stress_suite(opt_dir.as_deref()) {
            Ok(report) => {
                print!("{}", vvbook_emu::x3_twin::format_stress_report(&report));
                return;
            }
            Err(e) => {
                eprintln!("X3 Hardware Twin stress suite failed: {e}");
                process::exit(1);
            }
        }
    }

    let sdcard_dir = args
        .windows(2)
        .find(|w| w[0] == "--sdcard")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| PathBuf::from("./emu_sdcard"));
    let web_bind = args.iter().position(|a| a == "--web").map(|pos| {
        args.get(pos + 1)
            .filter(|v| !v.starts_with("--"))
            .cloned()
            .unwrap_or_else(|| "127.0.0.1:8080".to_string())
    });
    let explicit_vvdrop = args
        .windows(2)
        .find(|w| w[0] == "--vvdrop-bind")
        .map(|w| w[1].clone());
    let (vvdrop_bind, allow_fallback) = match explicit_vvdrop {
        Some(addr) => (addr, false),
        None => {
            let preferred = if web_bind.as_deref().is_some_and(|w| w.ends_with(":8383")) {
                "127.0.0.1:8384"
            } else {
                "127.0.0.1:8383"
            };
            (preferred.to_string(), true)
        }
    };

    let rt = match EmuRuntime::boot_at(&sdcard_dir, &sample_dir, &vvdrop_bind, allow_fallback) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Emulator boot failed: {e}");
            process::exit(1);
        }
    };

    if let Some(addr) = web_bind {
        if let Err(e) = run_web_emulator(rt, &addr) {
            eprintln!("Web emulator error: {e}");
            process::exit(1);
        }
        return;
    }

    println!(
        "VVBook Desktop Emulator starting (VVDrop at {})",
        rt.portal.base_url()
    );

    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("No GUI display available ({e}). Tip: run with `--web 127.0.0.1:8080` or `--snapshot-all <dir>`.");
            process::exit(1);
        }
    };
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut desktop = DesktopApp {
        rt,
        window: None,
        surface: None,
        cursor: (0.0, 0.0),
    };
    let _ = event_loop.run_app(&mut desktop);
}
