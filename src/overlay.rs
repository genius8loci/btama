//! Интерфейс оверлея и применение читов.
//!
//! # Отличия от прежней версии
//!
//! * `render` обёрнут в [`std::panic::catch_unwind`]. Паника в обработчике,
//!   вызванном из чужого `extern "system"`-хука, иначе завершает игру
//!   через `abort`; теперь оверлей просто выключается, а причина попадает
//!   в лог;
//! * настройки хранятся по `m_UniqueID`, а не по адресу объекта, который
//!   меняется при каждой сборке мусора;
//! * меню закрепляется на `HOME` и на время удержания показывается по
//!   `TAB`; пока оно открыто, ввод не доходит до игры (см.
//!   [`ImguiRenderLoop::message_filter`]);
//! * ползунки параметров больше не перезаписываются значением из памяти
//!   каждый кадр — текущее значение показано рядом отдельным текстом;
//! * мировые координаты переводятся в экранные матрицей камеры игры, а не
//!   ползунками: `Zoom`, `Cam X` и `Cam Y` остались лишь запасным путём на
//!   случай, когда камера не читается.

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;

use hudhook::{ImguiRenderLoop, MessageFilter, RenderContext};
use imgui::{Condition, Context, Drag, ImColor32, Io, MouseButton, StyleColor, Ui, WindowFlags};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::font;
use crate::game::{self, Affine, CameraView, PlayerView, RectF, TrapRef, World};
use crate::hook;
use crate::log;
use crate::offsets::trap;

const VK_TAB: i32 = 0x09;
/// `HOME` — закрепляет меню.
const VK_HOME: i32 = 0x24;
/// `PAGE DOWN` — выгружает DLL. В Win32 эта клавиша называется `VK_NEXT`
/// (0x22), а `VK_PRIOR` — это, наоборот, Page Up.
const VK_NEXT: i32 = 0x22;

/// Пауза между попытками установить хук, секунды.
const SCAN_INTERVAL: f32 = 3.0;

/// Сколько строк ловушек показывать: рисовать тысячу строк ImGui дороже,
/// чем есть от них пользы.
const TRAP_ROWS: usize = 30;

const ACCENT: [f32; 4] = [153.0 / 255.0, 102.0 / 255.0, 204.0 / 255.0, 1.0];
const MUTED: [f32; 4] = [0.55, 0.55, 0.55, 1.0];
const ALERT: [f32; 4] = [0.95, 0.45, 0.35, 1.0];

// ============================================================================
// СОСТОЯНИЕ
// ============================================================================

/// Настройки читов для одного игрока.
#[derive(Clone, Debug, Default)]
struct PlayerConfig {
    god_mode: bool,
    inf_jump: bool,
    override_speed: bool,
    speed: f32,
    override_jump: bool,
    jump_height: f32,
    override_gravity: bool,
    gravity: f32,
    /// Начальные значения подтягиваются из памяти ровно один раз, при первой
    /// встрече с игроком.
    seeded: bool,
}

impl PlayerConfig {
    fn seed(&mut self, stats: game::LiveStats) {
        // Пока хоть одно значение не прочиталось, объект ещё не готов —
        // отметить настройки заполненными нельзя, иначе на ползунках
        // навсегда останутся выдуманные числа.
        let (Some(speed), Some(jump_height), Some(gravity)) =
            (stats.speed, stats.jump_height, stats.gravity)
        else {
            return;
        };
        if self.seeded {
            return;
        }
        self.speed = speed;
        self.jump_height = jump_height;
        self.gravity = gravity;
        self.seeded = true;
    }
}

/// Откуда брать преобразование мировых координат в экранные.
///
/// По умолчанию — из камеры игры: её `m_Transform` и есть та матрица, с
/// которой игра рисует кадр, так что ESP совпадает с картинкой сам собой.
/// Ручные ползунки остались запасным путём на случай, если камера почему-то
/// не читается (например, до входа в уровень).
#[derive(Clone, Copy, Debug)]
struct View {
    follow_camera: bool,
    zoom: f32,
    camera_x: f32,
    camera_y: f32,
}

impl Default for View {
    fn default() -> Self {
        Self {
            follow_camera: true,
            zoom: 0.833,
            camera_x: 0.0,
            camera_y: 0.0,
        }
    }
}

impl View {
    fn manual_transform(self) -> Affine {
        Affine::from_zoom_and_offset(self.zoom, self.camera_x, self.camera_y)
    }
}

/// Стабильный цвет объекта по его адресу.
///
/// Нужен, чтобы одну и ту же ловушку было легко сопоставить между рамкой на
/// экране и строкой в списке. Биты адреса сначала перемешиваются: без этого
/// объекты, лежащие рядом в куче, получали бы почти одинаковый оттенок.
fn color_for(addr: usize) -> [f32; 3] {
    let mut hash = addr as u32;
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x7FEB_352D);
    hash ^= hash >> 15;
    hash = hash.wrapping_mul(0x846C_A68B);
    hash ^= hash >> 16;

    hsv_to_rgb((hash % 360) as f32, 0.7, 1.0)
}

fn hsv_to_rgb(hue: f32, saturation: f32, value: f32) -> [f32; 3] {
    let sector = (hue / 60.0).rem_euclid(6.0);
    let fraction = sector - sector.floor();
    let p = value * (1.0 - saturation);
    let q = value * (1.0 - saturation * fraction);
    let t = value * (1.0 - saturation * (1.0 - fraction));

    match sector as u32 {
        0 => [value, t, p],
        1 => [q, value, p],
        2 => [p, value, t],
        3 => [p, q, value],
        4 => [t, p, value],
        _ => [value, p, q],
    }
}

fn rgba(color: [f32; 3], alpha: f32) -> ImColor32 {
    ImColor32::from_rgba_f32s(color[0], color[1], color[2], alpha)
}

/// Отслеживание фронтов нажатия: `GetAsyncKeyState` сообщает лишь текущее
/// состояние, а нам нужны именно нажатия.
#[derive(Default)]
struct Keys {
    toggle_down: bool,
    eject_down: bool,
}

/// Что нажато в этом кадре.
struct Input {
    /// `HOME` только что нажат — переключить закрепление меню.
    toggle: bool,
    /// `TAB` удерживается — показывать меню, пока держат.
    peek: bool,
    /// `PAGE DOWN` только что нажат — выгрузить DLL.
    eject: bool,
}

impl Keys {
    fn poll(&mut self) -> Input {
        if !game_has_focus() {
            self.toggle_down = false;
            self.eject_down = false;
            return Input {
                toggle: false,
                peek: false,
                eject: false,
            };
        }

        let toggle = key_down(VK_HOME);
        let eject = key_down(VK_NEXT);
        let input = Input {
            toggle: toggle && !self.toggle_down,
            peek: key_down(VK_TAB),
            eject: eject && !self.eject_down,
        };
        self.toggle_down = toggle;
        self.eject_down = eject;
        input
    }
}

fn key_down(vk: i32) -> bool {
    // Старший бит означает «клавиша нажата сейчас».
    // SAFETY: обращение к пользовательскому вводу без побочных эффектов.
    unsafe { GetAsyncKeyState(vk) < 0 }
}

/// Активно ли сейчас окно нашего процесса.
///
/// Без этой проверки меню реагировало на клавиши, даже когда игра свёрнута.
fn game_has_focus() -> bool {
    // SAFETY: обе функции принимают произвольный HWND и сообщают об ошибке
    // возвращаемым значением.
    unsafe {
        let window = GetForegroundWindow();
        if window.0 == 0 {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, Some(&mut pid));
        pid != 0 && pid == GetCurrentProcessId()
    }
}

/// Главный оверлей.
pub struct CheatOverlay {
    /// Выставляется после паники: оверлей замолкает, игра продолжает работать.
    disabled: bool,

    // Переиспользуемые буферы: рендер не должен аллоцировать каждый кадр.
    objects: Vec<usize>,
    traps: Vec<TrapRef>,
    classes: Vec<(usize, usize)>,

    settings: HashMap<i32, PlayerConfig>,
    /// Классы ловушек, которые пользователь пометил как `BoomTrap`.
    boom_classes: HashSet<usize>,

    keys: Keys,
    view: View,
    /// Камера, прочитанная в этом кадре; `None` — работаем по ползункам.
    camera: Option<CameraView>,
    /// Преобразование, применяемое в этом кадре и к ESP, и к телепорту.
    /// Пересчитывается в начале каждого кадра, поэтому обе стороны
    /// заведомо согласованы между собой.
    transform: Affine,
    scan_timer: f32,

    menu_pinned: bool,
    menu_open: bool,
    show_esp: bool,
    show_trap_esp: bool,
    show_trap_menu: bool,
    /// Телепорт персонажа правой кнопкой мыши.
    teleport: bool,
    /// Показывать сырые значения рамок в списке ловушек.
    show_trap_rects: bool,
}

impl Default for CheatOverlay {
    fn default() -> Self {
        Self {
            disabled: false,
            objects: Vec::with_capacity(hook::MAX_OBJECTS),
            traps: Vec::with_capacity(128),
            classes: Vec::with_capacity(16),
            settings: HashMap::new(),
            boom_classes: HashSet::new(),
            keys: Keys::default(),
            view: View::default(),
            camera: None,
            transform: View::default().manual_transform(),
            scan_timer: 0.0,
            menu_pinned: false,
            menu_open: false,
            show_esp: true,
            show_trap_esp: true,
            show_trap_menu: false,
            teleport: false,
            show_trap_rects: false,
        }
    }
}

impl CheatOverlay {
    pub fn new() -> Self {
        Self::default()
    }
}

// ============================================================================
// РЕНДЕР
// ============================================================================

impl ImguiRenderLoop for CheatOverlay {
    /// Вызывается один раз, до построения текстуры атласа шрифтов, — это
    /// единственный момент, когда шрифт ещё можно заменить.
    fn initialize<'a>(&'a mut self, ctx: &mut Context, _render: &'a mut dyn RenderContext) {
        font::install(ctx);
    }

    fn render(&mut self, ui: &mut Ui) {
        if self.disabled {
            return;
        }
        // Паника не должна пересекать границу extern "system" — там она
        // превращается в abort и убивает игру вместе с чужим процессом.
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| self.render_frame(ui)));
        if result.is_err() {
            self.disabled = true;
            log::error!("паника в render, оверлей отключён (детали выше)");
            hook::uninstall();
            game::forget_cached_pointers();
        }
    }

    fn message_filter(&self, _io: &Io) -> MessageFilter {
        if self.menu_pinned {
            // Меню закреплено: перехватываем весь ввод, иначе клики уходят
            // и в интерфейс, и в игру одновременно.
            MessageFilter::InputAll
        } else if self.menu_open {
            // Режим подсматривания по TAB: мышь наша, управление — игры.
            MessageFilter::InputMouse
        } else {
            MessageFilter::empty()
        }
    }
}

impl CheatOverlay {
    fn render_frame(&mut self, ui: &Ui) {
        crate::mem::tick();

        let input = self.keys.poll();
        if input.toggle {
            self.menu_pinned = !self.menu_pinned;
        }
        if input.eject {
            log::info!("запрошена выгрузка по PAGE DOWN");
            // Выгрузка идёт в отдельном потоке, а рендер продолжает
            // вызываться. Замолкаем сразу, иначе можно обратиться к памяти
            // уже выгружаемой библиотеки.
            self.disabled = true;
            self.menu_open = false;
            self.menu_pinned = false;
            hook::uninstall();
            game::forget_cached_pointers();
            hudhook::eject();
            return;
        }
        self.menu_open = self.menu_pinned || input.peek;

        if self.menu_open {
            ui.set_mouse_cursor(Some(imgui::MouseCursor::Arrow));
        } else {
            ui.set_mouse_cursor(None);
        }

        // Хук: либо забираем объекты, либо пробуем поставить.
        if hook::is_installed() {
            hook::take_objects(&mut self.objects);
        } else {
            self.objects.clear();
            if hook::should_retry() {
                self.scan_timer += ui.io().delta_time;
                if self.scan_timer >= SCAN_INTERVAL {
                    self.scan_timer = 0.0;
                    hook::spawn_install_attempt();
                }
            }
        }

        let world = game::build_world(&self.objects);
        self.refresh_transform(ui, &world);

        // Ловушки читаются только когда действительно нужны.
        if world.screen != 0 && (self.show_trap_esp || self.show_trap_menu) {
            game::collect_traps(world.screen, &mut self.traps);
        } else {
            self.traps.clear();
        }
        self.refresh_classes();

        self.draw_esp(ui, &world);
        self.draw_main_window(ui, &world);
        if self.show_trap_menu && self.menu_open {
            self.draw_trap_window(ui);
        }
        if self.menu_open {
            self.handle_teleport(ui, &world);
            draw_cursor(ui);
        }
    }

    /// Обновляет преобразование мира в экран на этот кадр.
    ///
    /// Камера перечитывается каждый кадр по той же причине, по которой
    /// заново строится список игроков: сборщик мусора двигает объекты, и
    /// адрес камеры с прошлого кадра ничего не гарантирует.
    fn refresh_transform(&mut self, ui: &Ui, world: &World) {
        self.camera = self
            .view
            .follow_camera
            .then(|| game::read_camera(world.screen))
            .flatten();

        self.transform = match self.camera {
            Some(camera) => {
                let [width, height] = ui.io().display_size;
                camera
                    .transform
                    .fit_to_display(camera.viewport, (width, height))
            }
            None => self.view.manual_transform(),
        };
    }

    /// Телепорт персонажа в точку под курсором по правой кнопке.
    ///
    /// Работает только при видимом курсоре — то есть пока удерживают `TAB`
    /// или меню закреплено, — и только когда клик пришёлся мимо окон
    /// интерфейса: иначе правый клик по ползунку утаскивал бы персонажа.
    fn handle_teleport(&self, ui: &Ui, world: &World) {
        if !self.teleport || ui.io().want_capture_mouse {
            return;
        }
        if !ui.is_mouse_clicked(MouseButton::Right) {
            return;
        }
        let Some(local) = world.players.iter().find(|player| player.is_local) else {
            return;
        };

        let [x, y] = ui.io().mouse_pos;
        let (world_x, world_y) = self.transform.to_world(x, y);
        if game::set_position(local.addr, world_x, world_y) {
            log::info!("телепорт в ({world_x:.0}, {world_y:.0})");
        }
    }

    /// Пересчитывает список классов ловушек и их количества.
    fn refresh_classes(&mut self) {
        self.classes.clear();
        for item in &self.traps {
            match self
                .classes
                .iter_mut()
                .find(|(class, _)| *class == item.class)
            {
                Some((_, count)) => *count += 1,
                None => self.classes.push((item.class, 1)),
            }
        }
        self.classes
            .sort_unstable_by_key(|&(_, count)| std::cmp::Reverse(count));
    }

    // ------------------------------------------------------------------
    // ESP
    // ------------------------------------------------------------------

    fn draw_esp(&self, ui: &Ui, world: &World) {
        let draw_list = ui.get_background_draw_list();

        if self.show_trap_esp {
            for item in &self.traps {
                // Рамка уже проверена на правдоподобность при сборе.
                let Some(rect) = item.rect else {
                    continue;
                };
                // Тот же цвет, что и у фона названия в списке: так видно,
                // какая рамка какой строке соответствует.
                let color = rgba(color_for(item.addr), 0.85);
                let (top_left, bottom_right) = self.project(rect);
                draw_list
                    .add_rect(top_left, bottom_right, color)
                    .thickness(1.0)
                    .build();
            }
        }

        if self.show_esp {
            for player in &world.players {
                let Some(rect) = player.rect else {
                    continue;
                };
                let color = if player.is_local {
                    ImColor32::from_rgba(0, 255, 0, 200)
                } else {
                    ImColor32::from_rgba(255, 0, 0, 150)
                };
                let (top_left, bottom_right) = self.project(rect.into());
                draw_list
                    .add_rect(top_left, bottom_right, color)
                    .thickness(2.0)
                    .build();
            }
        }
    }

    /// Переводит мировую рамку в пару экранных углов.
    ///
    /// Углы противоположные, а не «левый верхний и правый нижний»: при
    /// повороте камеры прямоугольник перестаёт быть осевым, и правильнее
    /// было бы рисовать четырёхугольник. Игра камеру не поворачивает, так
    /// что осевой рамки достаточно, но `add_rect` требует упорядоченных
    /// углов — иначе рамка схлопывается.
    fn project(&self, rect: RectF) -> ([f32; 2], [f32; 2]) {
        let first = self.transform.to_screen(rect.x, rect.y);
        let second = self.transform.to_screen(rect.x + rect.w, rect.y + rect.h);
        (
            [first[0].min(second[0]), first[1].min(second[1])],
            [first[0].max(second[0]), first[1].max(second[1])],
        )
    }

    // ------------------------------------------------------------------
    // ГЛАВНОЕ ОКНО
    // ------------------------------------------------------------------

    fn draw_main_window(&mut self, ui: &Ui, world: &World) {
        let _bg = ui.push_style_color(StyleColor::WindowBg, [0.05, 0.05, 0.05, 0.9]);
        let _border = ui.push_style_color(StyleColor::Border, ACCENT);

        let flags = WindowFlags::NO_DECORATION
            | WindowFlags::ALWAYS_AUTO_RESIZE
            | WindowFlags::NO_MOVE
            | WindowFlags::NO_NAV;

        ui.window("##Overlay")
            .flags(flags)
            .position([20.0, 20.0], Condition::FirstUseEver)
            .build(|| {
                ui.text_colored(ACCENT, "BLOODY AMA TRAPLAND");

                if !hook::is_installed() {
                    self.draw_hook_status(ui);
                    return;
                }

                if self.menu_open {
                    self.draw_toolbar(ui, world);
                }

                if world.players.is_empty() {
                    ui.text_colored(MUTED, "Игроки не найдены");
                    return;
                }

                for player in &world.players {
                    self.draw_player_row(ui, player);
                }
            });
    }

    fn draw_hook_status(&mut self, ui: &Ui) {
        match hook::status() {
            hook::Status::Installed => {}
            hook::Status::Scanning => ui.text("Сканирование памяти..."),
            hook::Status::Idle | hook::Status::NotFound => {
                ui.text_colored(MUTED, "Ожидание игры: метод ещё не вызывался");
            }
            hook::Status::Ambiguous(count) => {
                ui.text_colored(
                    ALERT,
                    format!("Сигнатура неоднозначна ({count} совпадений)"),
                );
                ui.text_colored(MUTED, "Хук не установлен намеренно, см. лог");
            }
            hook::Status::Failed(reason) => {
                ui.text_colored(ALERT, format!("Ошибка MinHook: {reason}"));
            }
            hook::Status::GaveUp => {
                ui.text_colored(ALERT, "Сигнатура не найдена, попытки исчерпаны");
            }
        }
        if self.menu_open && ui.button("Искать снова") {
            self.scan_timer = 0.0;
            hook::force_retry();
        }
        if self.menu_open {
            ui.text_colored(MUTED, format!("Лог: {}", crate::log::log_path().display()));
        }
    }

    fn draw_toolbar(&mut self, ui: &Ui, world: &World) {
        ui.checkbox("ESP", &mut self.show_esp);
        ui.same_line();
        if world.screen != 0 {
            ui.checkbox("Ловушки", &mut self.show_trap_menu);
            ui.same_line();
        }
        ui.checkbox("Teleport", &mut self.teleport);
        if ui.is_item_hovered() {
            ui.tooltip_text("ПКМ мимо окон — телепорт персонажа в точку под курсором");
        }
        ui.same_line();
        ui.text_colored(MUTED, if self.menu_pinned { "[HOME]" } else { "[TAB]" });

        self.draw_diagnostics(ui, world);
        self.draw_view_controls(ui);
        ui.separator();
    }

    /// Управление преобразованием мира в экран.
    ///
    /// Пока камера читается, ползунки не показываются вовсе: они бы только
    /// сбивали с толку — ни на что они в этом режиме не влияют.
    fn draw_view_controls(&mut self, ui: &Ui) {
        ui.checkbox("Камера игры", &mut self.view.follow_camera);
        if ui.is_item_hovered() {
            ui.tooltip_text(
                "Брать преобразование из m_Transform камеры — той самой матрицы,\n\
                 с которой игра рисует кадр.\n\
                 Снимите, чтобы совместить рамки вручную.",
            );
        }

        match self.camera {
            Some(camera) => {
                ui.same_line();
                let source = if camera.from_matrix {
                    "m_Transform"
                } else {
                    "зум + позиция"
                };
                ui.text_colored(
                    MUTED,
                    format!(
                        "{source}: зум {:.3}, центр ({:.0}, {:.0}), вьюпорт {:.0}x{:.0}",
                        camera.zoom,
                        camera.position.0,
                        camera.position.1,
                        camera.viewport.0,
                        camera.viewport.1
                    ),
                );
            }
            None => {
                if self.view.follow_camera {
                    ui.same_line();
                    ui.text_colored(ALERT, "камера не читается, работают ползунки");
                }

                ui.set_next_item_width(90.0);
                ui.slider("Zoom", 0.1, 3.0, &mut self.view.zoom);
                ui.same_line();
                if ui.button("Сброс") {
                    self.view = View::default();
                }

                ui.set_next_item_width(90.0);
                Drag::new("Cam X")
                    .speed(1.0)
                    .build(ui, &mut self.view.camera_x);
                ui.same_line();
                ui.set_next_item_width(90.0);
                Drag::new("Cam Y")
                    .speed(1.0)
                    .build(ui, &mut self.view.camera_y);
            }
        }
    }

    /// Показывает сырые счётчики цепочки разбора.
    ///
    /// Когда список игроков пуст, по одной строке видно, где именно рвётся
    /// цепочка: хук не приносит объектов, не опознан класс, или не нашёлся
    /// объект с `isLocal == 1`.
    fn draw_diagnostics(&self, ui: &Ui, world: &World) {
        let class = match game::player_class() {
            Some(class) => format!("0x{class:X}"),
            None => "не опознан".to_string(),
        };
        let mode = match game::is_online(world.screen) {
            Some(true) => "сеть",
            Some(false) => "одиночная",
            None => "?",
        };
        let camera = match self.camera {
            Some(camera) => format!("0x{:X}", camera.addr),
            None => match game::camera(world.screen) {
                Some(addr) => format!("0x{addr:X} (не читается)"),
                None => "нет".to_string(),
            },
        };
        ui.text_colored(
            MUTED,
            format!(
                "объектов: {} | класс: {class} | экран: 0x{:X} ({mode}) | камера: {camera} | ловушек: {}",
                self.objects.len(),
                world.screen,
                self.traps.len()
            ),
        );
    }

    fn draw_player_row(&mut self, ui: &Ui, player: &PlayerView) {
        let _id = ui.push_id(player.unique_id.to_string());

        let name = if player.name.is_empty() {
            "Unknown"
        } else {
            player.name.as_str()
        };
        ui.text_colored(player.color, format!("{name} [ID:{}]", player.unique_id));
        ui.same_line();
        ui.text(format!(
            "({:.0}:{:.0})",
            player.position.0, player.position.1
        ));

        if !player.is_local {
            if self.menu_open {
                ui.same_line();
                ui.text_colored(MUTED, "Remote");
                ui.separator();
            }
            return;
        }

        let stats = game::read_live_stats(player.addr);
        let config = self.settings.entry(player.unique_id).or_default();
        config.seed(stats);

        if self.menu_open {
            ui.same_line();
            if ui.button("Respawn") {
                game::respawn(player.addr);
            }

            ui.checkbox("God", &mut config.god_mode);
            ui.same_line();
            ui.checkbox("Inf Jump", &mut config.inf_jump);

            slider_row(
                ui,
                "Speed",
                "##ospd",
                &mut config.override_speed,
                &mut config.speed,
                0.0,
                400.0,
                stats.speed,
            );
            slider_row(
                ui,
                "Jump H",
                "##ojmp",
                &mut config.override_jump,
                &mut config.jump_height,
                0.0,
                100.0,
                stats.jump_height,
            );
            slider_row(
                ui,
                "Gravity",
                "##ogrv",
                &mut config.override_gravity,
                &mut config.gravity,
                0.0,
                2.0,
                stats.gravity,
            );
        }

        // Эффекты применяются всегда, а не только при открытом меню.
        if config.god_mode {
            game::set_god_mode(player.addr);
        }
        if config.inf_jump {
            game::set_infinite_jump(player.addr);
        }
        if config.override_speed {
            game::set_speed(player.addr, config.speed);
        }
        if config.override_jump {
            game::set_jump_height(player.addr, config.jump_height);
        }
        if config.override_gravity {
            game::set_gravity(player.addr, config.gravity);
        }

        if self.menu_open {
            ui.separator();
        }
    }

    // ------------------------------------------------------------------
    // ОКНО ЛОВУШЕК
    // ------------------------------------------------------------------

    fn draw_trap_window(&mut self, ui: &Ui) {
        let _bg = ui.push_style_color(StyleColor::WindowBg, [0.05, 0.05, 0.05, 0.9]);
        let _border = ui.push_style_color(StyleColor::Border, ACCENT);

        ui.window("##TrapManager")
            .position([520.0, 20.0], Condition::FirstUseEver)
            .size([520.0, 460.0], Condition::FirstUseEver)
            .build(|| {
                ui.text_colored(ACCENT, "=== TRAP MANAGER ===");
                ui.checkbox("Рисовать ESP ловушек", &mut self.show_trap_esp);
                ui.same_line();
                ui.checkbox("Показать рамки", &mut self.show_trap_rects);
                if ui.is_item_hovered() {
                    ui.tooltip_text(
                        "Сырые источники рамки по каждой ловушке:\n\
                         поз — PositionX/PositionY (0x24), мировая позиция;\n\
                         0x50 — m_Bounding, коллизия в мировых координатах;\n\
                         0x70 — m_Rectangle, кадр в атласе текстур (не мир!);\n\
                         0x88 — вторая мировая позиция.",
                    );
                }
                ui.text(format!("Всего ловушек: {}", self.traps.len()));
                let with_rect = self.traps.iter().filter(|item| item.rect.is_some()).count();
                if with_rect != self.traps.len() {
                    ui.text_colored(
                        ALERT,
                        format!(
                            "Пригодная рамка есть только у {with_rect} из {} — остальные ESP не рисует",
                            self.traps.len()
                        ),
                    );
                }
                ui.separator();

                self.draw_trap_classes(ui);
                ui.separator();

                let shown = self.traps.len().min(TRAP_ROWS);
                if self.traps.len() > shown {
                    ui.text_colored(
                        MUTED,
                        format!("Показаны первые {shown} из {}", self.traps.len()),
                    );
                }
                for index in 0..shown {
                    let item = self.traps[index];
                    let _id = ui.push_id(index.to_string());
                    self.draw_trap_row(ui, index, item);
                }
            });
    }

    /// Классы ловушек и пометка «это BoomTrap».
    ///
    /// Смещения `Speed` и `m_CanTrigger` совпадают со вторым `m_Bounding`
    /// базового `Trap`, поэтому писать их можно только там, где класс
    /// действительно их содержит. Определить это автоматически нельзя —
    /// решает пользователь.
    fn draw_trap_classes(&mut self, ui: &Ui) {
        ui.text_colored(MUTED, "Классы (таблица методов x количество):");
        for &(class, count) in &self.classes {
            let _id = ui.push_id(class.to_string());
            let mut marked = self.boom_classes.contains(&class);
            if ui.checkbox(format!("0x{class:X} x{count}"), &mut marked) {
                if marked {
                    self.boom_classes.insert(class);
                } else {
                    self.boom_classes.remove(&class);
                }
            }
            if ui.is_item_hovered() {
                ui.tooltip_text(
                    "Отметить, если это BoomTrap-подобный класс: откроет Speed (0xA4) и \
                     CanTrigger (0xAC).\nУ обычного Trap по этим адресам лежит прямоугольник \
                     коллизии — запись его испортит.",
                );
            }
        }
    }

    fn draw_trap_row(&mut self, ui: &Ui, index: usize, item: TrapRef) {
        let name = game::trap_name(item.addr).unwrap_or_else(|| format!("Trap #{index}"));
        highlighted_label(ui, &format!("{name:<20}"), color_for(item.addr));
        ui.same_line();

        flag_checkbox(ui, "U", "Used", item.addr, trap::USED);
        ui.same_line();
        flag_checkbox(ui, "Up", "Updateable", item.addr, trap::UPDATEABLE);
        ui.same_line();
        flag_checkbox(ui, "G", "GoreStick", item.addr, trap::GORE_STICK);

        if self.boom_classes.contains(&item.class) && game::supports_boom_fields(item.addr) {
            ui.same_line();
            flag_checkbox(
                ui,
                "T",
                "CanTrigger (BoomTrap)",
                item.addr,
                trap::BOOM_CAN_TRIGGER,
            );

            if let Some(mut speed) = game::trap_speed(item.addr) {
                ui.same_line();
                ui.set_next_item_width(60.0);
                if Drag::new("##speed")
                    .speed(0.1)
                    .range(0.0, 50.0)
                    .build(ui, &mut speed)
                {
                    game::set_trap_speed(item.addr, speed);
                }
                if ui.is_item_hovered() {
                    ui.tooltip_text("Speed (BoomTrap)");
                }
            }
        }

        // Отдельной строкой в конце, чтобы не ломать выкладку кнопок выше.
        if self.show_trap_rects {
            ui.text_colored(
                MUTED,
                format!(
                    "    поз {} | 0x50 {} | 0x70 {} | 0x88 {}",
                    format_point(item.position),
                    format_rect(item.bounding_raw),
                    format_rect(item.source_raw),
                    format_point(item.position_alt),
                ),
            );
        }
    }
}

// ============================================================================
// МЕЛКИЕ ЭЛЕМЕНТЫ
// ============================================================================

/// Текст на цветной подложке.
///
/// Подложка приглушена до трети непрозрачности: сам текст должен оставаться
/// читаемым, а цвет нужен лишь как метка, связывающая строку с рамкой на
/// экране.
fn highlighted_label(ui: &Ui, text: &str, color: [f32; 3]) {
    const PADDING: f32 = 3.0;

    let position = ui.cursor_screen_pos();
    let size = ui.calc_text_size(text);
    ui.get_window_draw_list()
        .add_rect(
            [position[0] - PADDING, position[1]],
            [position[0] + size[0] + PADDING, position[1] + size[1]],
            rgba(color, 0.35),
        )
        .filled(true)
        .build();
    ui.text(text);
}

/// Форматирует рамку для диагностики.
fn format_rect(rect: Option<game::Rect>) -> String {
    match rect {
        Some(rect) => format!("{},{} {}x{}", rect.x, rect.y, rect.w, rect.h),
        None => "-".to_string(),
    }
}

/// Форматирует точку для диагностики.
fn format_point(point: Option<(f32, f32)>) -> String {
    match point {
        Some((x, y)) => format!("{x:.0},{y:.0}"),
        None => "-".to_string(),
    }
}

/// Чекбокс поверх байтового поля в памяти игры.
///
/// Если объект исчез, чтение вернёт `None`, и строка просто не отрисуется —
/// вместо записи по мёртвому адресу.
fn flag_checkbox(ui: &Ui, label: &str, tooltip: &str, addr: usize, offset: usize) {
    let Some(mut value) = game::trap_flag(addr, offset) else {
        ui.text_colored(MUTED, "-");
        return;
    };
    if ui.checkbox(label, &mut value) {
        game::set_trap_flag(addr, offset, value);
    }
    if ui.is_item_hovered() {
        ui.tooltip_text(tooltip);
    }
}

/// Строка «галка оверрайда + ползунок + текущее значение из памяти».
///
/// Ползунок хранит желаемое значение и не перезаписывается из памяти —
/// в прежней версии его нельзя было сдвинуть, пока не включён оверрайд.
#[allow(clippy::too_many_arguments)]
fn slider_row(
    ui: &Ui,
    label: &str,
    toggle_id: &str,
    enabled: &mut bool,
    value: &mut f32,
    min: f32,
    max: f32,
    live: Option<f32>,
) {
    ui.checkbox(toggle_id, enabled);
    ui.same_line();
    ui.set_next_item_width(160.0);
    ui.slider(label, min, max, value);
    ui.same_line();
    match live {
        Some(current) => ui.text_colored(MUTED, format!("= {current:.2}")),
        None => ui.text_colored(MUTED, "= ?"),
    }
}

/// Перекрестье под курсором: системный курсор поверх DirectX-окна виден не
/// всегда.
fn draw_cursor(ui: &Ui) {
    let draw_list = ui.get_foreground_draw_list();
    let [x, y] = ui.io().mouse_pos;
    let size = 8.0;
    let color = ImColor32::from_rgba(153, 102, 204, 255);

    draw_list
        .add_line([x - size, y], [x + size, y], color)
        .thickness(1.5)
        .build();
    draw_list
        .add_line([x, y - size], [x, y + size], color)
        .thickness(1.5)
        .build();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_view_round_trips() {
        let view = View {
            follow_camera: false,
            zoom: 0.833,
            camera_x: 120.0,
            camera_y: -45.0,
        };
        let transform = view.manual_transform();
        let [screen_x, screen_y] = transform.to_screen(234.0, 588.0);
        let (world_x, world_y) = transform.to_world(screen_x, screen_y);
        assert!((world_x - 234.0).abs() < 0.01, "получили {world_x}");
        assert!((world_y - 588.0).abs() < 0.01, "получили {world_y}");
    }

    #[test]
    fn zero_zoom_does_not_produce_infinity() {
        // Ползунок масштаба доходит до нуля, а телепорт делит на него.
        let view = View {
            follow_camera: false,
            zoom: 0.0,
            camera_x: 0.0,
            camera_y: 0.0,
        };
        let (x, y) = view.manual_transform().to_world(100.0, 200.0);
        assert!(x.is_finite() && y.is_finite());
    }

    /// Отражённое преобразование дало бы `add_rect` углы в обратном порядке,
    /// и рамка схлопнулась бы в ничто.
    #[test]
    fn projection_orders_the_corners() {
        let mut overlay = CheatOverlay::new();
        overlay.transform = Affine::from_zoom_and_offset(-1.0, 0.0, 0.0);
        let (top_left, bottom_right) = overlay.project(RectF {
            x: 100.0,
            y: 200.0,
            w: 48.0,
            h: 48.0,
        });
        assert!(top_left[0] < bottom_right[0] && top_left[1] < bottom_right[1]);
    }

    #[test]
    fn trap_color_is_stable_for_the_same_address() {
        assert_eq!(color_for(0x23CA_4A10), color_for(0x23CA_4A10));
    }

    #[test]
    fn neighbouring_addresses_get_distinguishable_colors() {
        // Объекты в куче лежат вплотную; без перемешивания битов их оттенки
        // были бы неразличимы.
        let first = color_for(0x0400_0000);
        let second = color_for(0x0400_0010);
        let distance: f32 = (0..3).map(|i| (first[i] - second[i]).abs()).sum();
        assert!(
            distance > 0.2,
            "цвета слишком близки: {first:?} и {second:?}"
        );
    }

    #[test]
    fn hsv_covers_the_primaries() {
        assert_eq!(hsv_to_rgb(0.0, 1.0, 1.0), [1.0, 0.0, 0.0]);
        assert_eq!(hsv_to_rgb(120.0, 1.0, 1.0), [0.0, 1.0, 0.0]);
        assert_eq!(hsv_to_rgb(240.0, 1.0, 1.0), [0.0, 0.0, 1.0]);
    }

    #[test]
    fn hsv_channels_stay_in_range_across_the_wheel() {
        for hue in 0..720 {
            let rgb = hsv_to_rgb(hue as f32, 0.7, 1.0);
            assert!(
                rgb.iter().all(|channel| (0.0..=1.0).contains(channel)),
                "оттенок {hue} дал {rgb:?}"
            );
        }
    }
}
