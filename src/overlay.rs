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
//! * меню закрепляется на `INSERT` и на время удержания показывается по
//!   `TAB`; пока оно открыто, ввод не доходит до игры (см.
//!   [`ImguiRenderLoop::message_filter`]);
//! * ползунки параметров больше не перезаписываются значением из памяти
//!   каждый кадр — текущее значение показано рядом отдельным текстом.

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;

use hudhook::{ImguiRenderLoop, MessageFilter};
use imgui::{Condition, Drag, ImColor32, Io, StyleColor, Ui, WindowFlags};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::game::{self, PlayerView, TrapRef, World};
use crate::hook;
use crate::log;
use crate::offsets::trap;

const VK_TAB: i32 = 0x09;
const VK_INSERT: i32 = 0x2D;
const VK_END: i32 = 0x23;

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

/// Преобразование мировых координат в экранные.
///
/// Настоящей матрицы камеры у нас нет: в снятых дампах структур её полей не
/// оказалось, поэтому масштаб и сдвиг задаются вручную. Формула при этом
/// записана как полноценное преобразование камеры, так что, когда смещения
/// найдутся, их достаточно будет подставить сюда.
#[derive(Clone, Copy, Debug)]
struct View {
    zoom: f32,
    camera_x: f32,
    camera_y: f32,
}

impl Default for View {
    fn default() -> Self {
        Self {
            zoom: 0.833,
            camera_x: 0.0,
            camera_y: 0.0,
        }
    }
}

impl View {
    fn to_screen(self, x: f32, y: f32) -> [f32; 2] {
        [
            (x - self.camera_x) * self.zoom,
            (y - self.camera_y) * self.zoom,
        ]
    }
}

/// Отслеживание фронтов нажатия: `GetAsyncKeyState` сообщает лишь текущее
/// состояние, а нам нужны именно нажатия.
#[derive(Default)]
struct Keys {
    insert_down: bool,
    end_down: bool,
}

impl Keys {
    /// Возвращает `(insert_pressed, tab_held, end_pressed)`.
    fn poll(&mut self) -> (bool, bool, bool) {
        if !game_has_focus() {
            self.insert_down = false;
            self.end_down = false;
            return (false, false, false);
        }

        let insert = key_down(VK_INSERT);
        let end = key_down(VK_END);
        let insert_pressed = insert && !self.insert_down;
        let end_pressed = end && !self.end_down;
        self.insert_down = insert;
        self.end_down = end;
        (insert_pressed, key_down(VK_TAB), end_pressed)
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
    scan_timer: f32,

    menu_pinned: bool,
    menu_open: bool,
    show_esp: bool,
    show_trap_esp: bool,
    show_trap_menu: bool,
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
            scan_timer: 0.0,
            menu_pinned: false,
            menu_open: false,
            show_esp: true,
            show_trap_esp: true,
            show_trap_menu: false,
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

        let (insert_pressed, tab_held, end_pressed) = self.keys.poll();
        if insert_pressed {
            self.menu_pinned = !self.menu_pinned;
        }
        if end_pressed {
            log::info!("запрошена выгрузка по END");
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
        self.menu_open = self.menu_pinned || tab_held;

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
            draw_cursor(ui);
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
            let color = ImColor32::from_rgba(255, 255, 0, 150);
            for item in &self.traps {
                let Some(rect) = item.rect.filter(game::Rect::is_plausible) else {
                    continue;
                };
                let top_left = self.view.to_screen(rect.x as f32, rect.y as f32);
                let bottom_right = self
                    .view
                    .to_screen((rect.x + rect.w) as f32, (rect.y + rect.h) as f32);
                draw_list
                    .add_rect(top_left, bottom_right, color)
                    .thickness(1.0)
                    .build();
            }
        }

        if self.show_esp {
            for player in &world.players {
                let Some(rect) = player.rect.filter(game::Rect::is_plausible) else {
                    continue;
                };
                let color = if player.is_local {
                    ImColor32::from_rgba(0, 255, 0, 200)
                } else {
                    ImColor32::from_rgba(255, 0, 0, 150)
                };
                let top_left = self.view.to_screen(rect.x as f32, rect.y as f32);
                let bottom_right = self
                    .view
                    .to_screen((rect.x + rect.w) as f32, (rect.y + rect.h) as f32);
                draw_list
                    .add_rect(top_left, bottom_right, color)
                    .thickness(2.0)
                    .build();
            }
        }
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
        ui.text_colored(
            MUTED,
            if self.menu_pinned {
                "[закреплено]"
            } else {
                "[TAB]"
            },
        );

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

        ui.separator();
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
                ui.text(format!("Всего ловушек: {}", self.traps.len()));
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
        ui.text(format!("{name:<20}"));
        ui.same_line();

        flag_checkbox(ui, "U", "Used", item.addr, trap::USED);
        ui.same_line();
        flag_checkbox(ui, "Up", "Updateable", item.addr, trap::UPDATEABLE);
        ui.same_line();
        flag_checkbox(ui, "G", "GoreStick", item.addr, trap::GORE_STICK);

        if !self.boom_classes.contains(&item.class) || !game::supports_boom_fields(item.addr) {
            return;
        }

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
}

// ============================================================================
// МЕЛКИЕ ЭЛЕМЕНТЫ
// ============================================================================

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
