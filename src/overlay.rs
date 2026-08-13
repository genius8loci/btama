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
//! * ползунки параметров действуют с первого сдвига и держат значение сами,
//!   без отдельной галочки «применять»; вернуть исходное можно кнопкой;
//! * список ловушек сгруппирован по классам: у объектов одного класса
//!   совпадают и поля, и смысл, поэтому действие над одним осмысленно ровно
//!   настолько же, насколько над всеми;
//! * мировые координаты переводятся в экранные матрицей камеры игры, а не
//!   ползунками: `Zoom`, `Cam X` и `Cam Y` остались лишь запасным путём на
//!   случай, когда камера не читается.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;

use hudhook::{ImguiRenderLoop, MessageFilter, RenderContext};
use imgui::{Condition, Context, Drag, ImColor32, Io, MouseButton, StyleColor, Ui, WindowFlags};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::font;
use crate::game::{self, Affine, CameraView, PlayerView, RectF, TrapRef, TrapRole, World};
use crate::hook;
use crate::log;
use crate::offsets::trap;

const VK_TAB: i32 = 0x09;
/// `HOME` — закрепляет меню.
const VK_HOME: i32 = 0x24;
/// `PAGE DOWN` — выгружает DLL. В Win32 эта клавиша называется `VK_NEXT`
/// (0x22), а `VK_PRIOR` — это, наоборот, Page Up.
const VK_NEXT: i32 = 0x22;

/// Клавиши движения — ровно те, что читает сама игра в `HandleInput`.
const MOVE_LEFT_KEYS: [i32; 2] = [0x25, 0x41];
/// Стрелка вправо и `D`.
const MOVE_RIGHT_KEYS: [i32; 2] = [0x27, 0x44];

/// Пауза между попытками установить хук, секунды.
const SCAN_INTERVAL: f32 = 3.0;

const ACCENT: [f32; 4] = [153.0 / 255.0, 102.0 / 255.0, 204.0 / 255.0, 1.0];
const MUTED: [f32; 4] = [0.55, 0.55, 0.55, 1.0];
const ALERT: [f32; 4] = [0.95, 0.45, 0.35, 1.0];

// ============================================================================
// СОСТОЯНИЕ
// ============================================================================

/// Один числовой параметр персонажа.
///
/// Галочки «применять» больше нет: ползунок начинает действовать с первого
/// же сдвига и с этого момента прописывается каждый кадр. Именно каждый —
/// иначе игра вернёт своё при ближайшем респавне, и «изменил и зафиксировал»
/// превратится в «изменил на секунду».
#[derive(Clone, Copy, Debug, Default)]
struct Tweak {
    /// Желаемое значение — то, что показывает ползунок.
    value: f32,
    /// Значение, каким оно было при первой встрече с персонажем; к нему
    /// возвращает кнопка «Сброс».
    original: f32,
    /// Трогал ли пользователь ползунок. Пока нет — в память не пишем.
    active: bool,
}

impl Tweak {
    fn seed(&mut self, live: f32) {
        self.value = live;
        self.original = live;
    }

    /// Ползунок со сбросом. Возвращает значение, если его нужно записать.
    fn draw(&mut self, ui: &Ui, label: &str, min: f32, max: f32, live: Option<f32>) -> Option<f32> {
        ui.set_next_item_width(160.0);
        if ui.slider(label, min, max, &mut self.value) {
            self.active = true;
        }
        ui.same_line();
        if ui.button(format!("Сброс##{label}")) {
            self.value = self.original;
            self.active = false;
            // Однократная запись: снявшись с фиксации, дальше мы молчим,
            // и без неё в памяти осталось бы последнее выставленное число.
            return Some(self.original);
        }
        ui.same_line();
        match live {
            Some(current) => ui.text_colored(MUTED, format!("= {current:.2}")),
            None => ui.text_colored(MUTED, "= ?"),
        }
        self.active.then_some(self.value)
    }
}

/// Настройки читов для одного игрока.
#[derive(Clone, Debug, Default)]
struct PlayerConfig {
    god_mode: bool,
    inf_jump: bool,
    speed: Tweak,
    jump_height: Tweak,
    gravity: Tweak,
    /// Снимать запрет на движение вбок, пока персонаж пригнулся.
    crouch_walk: bool,
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
        self.speed.seed(speed);
        self.jump_height.seed(jump_height);
        self.gravity.seed(gravity);
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
        if window.0.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, Some(&mut pid));
        pid != 0 && pid == GetCurrentProcessId()
    }
}

/// Ловушки одного класса.
///
/// Раньше список был плоским: сорок строк, в которых одна и та же ловушка
/// повторялась десяток раз, и каждую приходилось щёлкать отдельно. Класс —
/// естественная единица: у его объектов совпадают и поля, и смысл, так что
/// действие над одним осмысленно ровно настолько же, насколько над всеми.
struct TrapGroup {
    /// Указатель на таблицу методов — он же ключ в `class_info`.
    class: usize,
    /// Индексы объектов этого класса в `traps`.
    members: Vec<usize>,
}

/// Состояние конвейера, каким оно попало в лог в последний раз.
///
/// Умышленно без `objects`: их число меняется каждый кадр, и снимок с ним
/// всегда считался бы новым.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Snapshot {
    hooked: bool,
    screen: usize,
    players: usize,
    traps: usize,
    traps_with_rect: usize,
    camera: Option<usize>,
    from_matrix: bool,
    sim_scale: i32,
    display: (i32, i32),
}

/// Главный оверлей.
pub struct CheatOverlay {
    /// Выставляется после паники: оверлей замолкает, игра продолжает работать.
    disabled: bool,

    // Переиспользуемые буферы: рендер не должен аллоцировать каждый кадр.
    objects: Vec<usize>,
    traps: Vec<TrapRef>,
    /// Ловушки, разложенные по классам. Действия применяются к группе
    /// целиком: у объектов одного класса одни и те же поля и один смысл.
    groups: Vec<TrapGroup>,

    settings: HashMap<i32, PlayerConfig>,
    /// Сведения о встреченных классах: тип из `ObjectType`, пример имени и
    /// выведенная из типа роль. Растёт по мере появления новых классов и
    /// живёт до конца сессии — таблицы методов сборщик мусора не двигает.
    class_info: HashMap<usize, game::ClassInfo>,

    keys: Keys,
    view: View,
    /// Камера, прочитанная в этом кадре; `None` — работаем по ползункам.
    camera: Option<CameraView>,
    /// Преобразование, применяемое в этом кадре и к ESP, и к телепорту.
    /// Пересчитывается в начале каждого кадра, поэтому обе стороны
    /// заведомо согласованы между собой.
    transform: Affine,
    /// Сколько пикселей мира в единице физического движка — выведено из
    /// самих ловушек, показывается в их окне.
    sim_scale: f32,
    /// Последнее состояние, записанное в лог. См. [`CheatOverlay::report_state`].
    reported: Snapshot,
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
    /// Подписывать рамки ловушек именем объекта.
    show_trap_labels: bool,
    /// Показывать сырые поля объекта за общей частью раскладки.
    show_raw_fields: bool,
}

impl Default for CheatOverlay {
    fn default() -> Self {
        Self {
            disabled: false,
            objects: Vec::with_capacity(hook::MAX_OBJECTS),
            traps: Vec::with_capacity(128),
            groups: Vec::with_capacity(16),
            settings: HashMap::new(),
            class_info: HashMap::new(),
            keys: Keys::default(),
            view: View::default(),
            camera: None,
            transform: View::default().manual_transform(),
            sim_scale: 0.0,
            reported: Snapshot::default(),
            scan_timer: 0.0,
            menu_pinned: false,
            menu_open: false,
            show_esp: true,
            show_trap_esp: true,
            show_trap_menu: false,
            teleport: false,
            show_trap_rects: false,
            show_trap_labels: false,
            show_raw_fields: false,
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
            self.sim_scale =
                game::collect_traps(world.screen, &mut self.class_info, &mut self.traps);
        } else {
            self.traps.clear();
        }
        self.refresh_groups();
        self.report_state(ui, &world);

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

    /// Пишет в лог состояние конвейера — но только когда оно изменилось.
    ///
    /// Строка в кадр забила бы файл за минуту и утонула бы в собственном
    /// шуме. Поэтому сравнивается снимок из полей, которые внутри уровня
    /// стоят на месте; число объектов от хука в него не входит — оно скачет
    /// каждый кадр, — но в саму строку попадает.
    fn report_state(&mut self, ui: &Ui, world: &World) {
        let [width, height] = ui.io().display_size;
        let snapshot = Snapshot {
            hooked: hook::is_installed(),
            screen: world.screen,
            players: world.players.len(),
            traps: self.traps.len(),
            traps_with_rect: self.traps.iter().filter(|item| item.rect.is_some()).count(),
            camera: self.camera.map(|camera| camera.addr),
            from_matrix: self.camera.is_some_and(|camera| camera.from_matrix),
            // Округляем: шум в последних разрядах не должен считаться
            // изменением состояния.
            sim_scale: self.sim_scale.round() as i32,
            display: (width as i32, height as i32),
        };
        if snapshot == self.reported {
            return;
        }
        self.reported = snapshot;

        log::info!(
            "состояние: хук={} экран=0x{:X} объектов={} игроков={} ловушек={} (с рамкой {}) \
             камера={} масштаб={} окно={}x{}",
            if snapshot.hooked { "да" } else { "нет" },
            snapshot.screen,
            self.objects.len(),
            snapshot.players,
            snapshot.traps,
            snapshot.traps_with_rect,
            match snapshot.camera {
                Some(addr) if snapshot.from_matrix => format!("0x{addr:X} (m_Transform)"),
                Some(addr) => format!("0x{addr:X} (собрана из полей)"),
                None => "нет".to_string(),
            },
            snapshot.sim_scale,
            snapshot.display.0,
            snapshot.display.1,
        );

        if snapshot.traps_with_rect != snapshot.traps {
            log::warning!(
                "{} ловушек без пригодной рамки — ESP их не рисует",
                snapshot.traps - snapshot.traps_with_rect
            );
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
    fn refresh_groups(&mut self) {
        self.groups.clear();
        for (index, item) in self.traps.iter().enumerate() {
            match self
                .groups
                .iter_mut()
                .find(|group| group.class == item.class)
            {
                Some(group) => group.members.push(index),
                None => self.groups.push(TrapGroup {
                    class: item.class,
                    members: vec![index],
                }),
            }
        }
        // Самые многочисленные наверх: в них обычно и есть смысл лезть.
        self.groups
            .sort_unstable_by_key(|group| std::cmp::Reverse(group.members.len()));
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

                // Подпись отвечает на вопрос «что это за пустой квадрат»:
                // у спавнов, финишей и зон нет спрайта, но тип есть. Берётся
                // из кэша классов — читать строки каждый кадр на каждый
                // объект незачем, тип у всего класса один.
                if self.show_trap_labels
                    && let Some(info) = self.class_info.get(&item.class)
                    && !info.label().is_empty()
                {
                    draw_list.add_text(
                        [top_left[0], top_left[1] - ui.current_font_size()],
                        color,
                        info.label(),
                    );
                }
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
                ui.text_colored(ACCENT, "BT AMA");

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
            draw_log_location(ui);
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
        draw_log_location(ui);
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
            ui.same_line();
            ui.checkbox("Ходить в приседе", &mut config.crouch_walk);
            if ui.is_item_hovered() {
                ui.tooltip_text(
                    "Взводит m_MovePlayerAnyhow (0x151), пока персонаж пригнулся.\n\
                     Поле выбрано по имени — если не сработает, поищите нужный флаг\n\
                     галочками ниже: они показывают и правят живые значения.",
                );
            }

            draw_crouch_flags(ui, player.addr);

            // Ползунки рисуются только при открытом меню, а применяются
            // всегда — значения хранит `config`, а не интерфейс.
            if let Some(value) = config.speed.draw(ui, "Speed", 0.0, 400.0, stats.speed) {
                game::set_speed(player.addr, value);
            }
            if let Some(value) =
                config
                    .jump_height
                    .draw(ui, "Jump H", 0.0, 100.0, stats.jump_height)
            {
                game::set_jump_height(player.addr, value);
            }
            if let Some(value) = config.gravity.draw(ui, "Gravity", 0.0, 2.0, stats.gravity) {
                game::set_gravity(player.addr, value);
            }
        }

        // Эффекты применяются всегда, а не только при открытом меню.
        if config.god_mode {
            game::set_god_mode(player.addr);
        }
        if config.inf_jump {
            game::set_infinite_jump(player.addr);
        }
        if config.speed.active {
            game::set_speed(player.addr, config.speed.value);
        }
        if config.jump_height.active {
            game::set_jump_height(player.addr, config.jump_height.value);
        }
        if config.gravity.active {
            game::set_gravity(player.addr, config.gravity.value);
        }
        // Только пока персонаж действительно пригнулся и просит идти: вне
        // приседа игра справляется сама, и вмешательство только мешало бы
        // её собственному разгону и трению.
        if config.crouch_walk
            && game::is_crouching(player.addr)
            && let Some(direction) = movement_direction(ui)
        {
            game::set_crouch_movement(player.addr, direction);
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
        // Заголовок переехал в шапку окна — к стрелке сворачивания, — поэтому
        // саму шапку надо покрасить: по умолчанию она синяя и с остальным
        // оформлением не сочетается.
        let _title = ui.push_style_color(StyleColor::TitleBg, [0.08, 0.05, 0.11, 0.95]);
        let _title_active =
            ui.push_style_color(StyleColor::TitleBgActive, [0.12, 0.08, 0.17, 0.95]);

        // Текст до `##` — видимая часть заголовка, после — идентификатор
        // окна: он обязан пережить переименование, иначе ImGui забудет
        // положение и размер окна.
        ui.window("Менеджер Ловушек##TrapManager")
            .position([520.0, 20.0], Condition::FirstUseEver)
            .size([520.0, 460.0], Condition::FirstUseEver)
            .build(|| {
                ui.checkbox("Рисовать ESP ловушек", &mut self.show_trap_esp);
                ui.same_line();
                ui.checkbox("Показать рамки", &mut self.show_trap_rects);
                if ui.is_item_hovered() {
                    ui.tooltip_text(
                        "Сырые источники рамки по каждому объекту класса:\n\
                         поз — PositionX/PositionY (0x24), в этой сборке всегда 0;\n\
                         0x50 — m_Bounding, коллизия в пикселях мира;\n\
                         0x70 — m_Rectangle, кадр в атласе текстур (не мир!);\n\
                         0x88 — Position в единицах физического движка.",
                    );
                }
                ui.same_line();
                ui.checkbox("Подписи", &mut self.show_trap_labels);
                ui.same_line();
                ui.checkbox("Сырые поля", &mut self.show_raw_fields);
                if ui.is_item_hovered() {
                    ui.tooltip_text(
                        "Дамп 0x90..0x100 у первого объекта каждого класса.\n\
                         Раскладка Spinner, Trampoline, TrapBlock и прочих новых типов\n\
                         не снята, гадать смещения и писать по ним нельзя — а прочитать\n\
                         и посмотреть глазами можно.",
                    );
                }
                ui.text(format!(
                    "Всего ловушек: {} | единица симуляции = {:.1} px",
                    self.traps.len(),
                    self.sim_scale
                ));
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

                for index in 0..self.groups.len() {
                    let _id = ui.push_id(index.to_string());
                    self.draw_trap_group(ui, index);
                }
            });
    }

    /// Одна строка на класс: тип, имя ассета, количество и общие флаги.
    ///
    /// Флаги показывают состояние первого объекта группы, а записываются во
    /// все сразу. Если объекты разошлись, к метке добавляется звёздочка —
    /// клик тогда не «переключает», а выравнивает группу.
    ///
    /// Индекс, а не ссылка на группу: рисование читает `self.traps` и
    /// `self.class_info`, и одновременное заимствование группы ссылкой
    /// заставило бы копировать её каждый кадр.
    fn draw_trap_group(&self, ui: &Ui, index: usize) {
        let group = &self.groups[index];
        let Some(&first) = group.members.first() else {
            return;
        };
        let item = self.traps[first];
        let addresses: Vec<usize> = group
            .members
            .iter()
            .map(|&index| self.traps[index].addr)
            .collect();

        let info = self.class_info.get(&group.class);
        let kind = info.map(|info| info.label()).unwrap_or("");
        let name = info.map(|info| info.sample_name.as_str()).unwrap_or("");

        highlighted_label(
            ui,
            &format!("{:<12}", if kind.is_empty() { "?" } else { kind }),
            color_for(group.class),
        );
        ui.same_line();
        ui.text_colored(MUTED, format!("{name:<20} x{}", group.members.len()));
        if ui.is_item_hovered() {
            ui.tooltip_text(format!(
                "ObjectType (0x10): {}
Name (0x20): {}
TextureName (0x0C): {}
ZoneName (0x1C): {}
                 Таблица методов: 0x{:X}

Читаем поля: {}
Объектов на уровне: {}",
                display_or_dash(kind),
                display_or_dash(name),
                display_or_dash(info.map(|info| info.texture.as_str()).unwrap_or("")),
                display_or_dash(info.map(|info| info.zone.as_str()).unwrap_or("")),
                group.class,
                match info.map(|info| info.role).unwrap_or_default() {
                    TrapRole::Base => "только WorldObject, до 0x90",
                    TrapRole::Trap => "WorldObject + m_Bounding (0xA4), TextureSize (0x94)",
                    TrapRole::Boom =>
                        "WorldObject + Speed (0xA4), CanTrigger (0xAC), координаты движения",
                    TrapRole::Spinner =>
                        "WorldObject; зона поражения считается из позиции и поворота",
                },
                group.members.len(),
            ));
        }
        ui.same_line();

        group_flag(ui, "U", "Used", &addresses, trap::USED);
        ui.same_line();
        group_flag(ui, "Up", "Updateable", &addresses, trap::UPDATEABLE);
        ui.same_line();
        group_flag(ui, "G", "GoreStick", &addresses, trap::GORE_STICK);

        if item.boom.is_some() {
            ui.same_line();
            group_flag(
                ui,
                "T",
                "CanTrigger (BoomTrap)",
                &addresses,
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
                    for &addr in &addresses {
                        game::set_trap_speed(addr, speed);
                    }
                }
                if ui.is_item_hovered() {
                    ui.tooltip_text("Speed (BoomTrap) — пишется во все объекты класса");
                }
            }
        }

        if self.show_trap_rects {
            for &member in &group.members {
                self.draw_trap_numbers(ui, self.traps[member]);
            }
        }
        if self.show_raw_fields {
            draw_raw_fields(ui, item.addr);
        }
    }

    /// Сырые источники рамки по одному объекту.
    fn draw_trap_numbers(&self, ui: &Ui, item: TrapRef) {
        ui.text_colored(
            MUTED,
            format!(
                "    0x50 {} | 0x70 кадр {} | 0x88 симуляция {} | масштаб {} | поворот {}",
                format_rect(item.bounding_raw),
                format_rect(item.source_raw),
                format_point(item.position_alt),
                format_scalar(item.scale),
                format_scalar(item.rotation),
            ),
        );
        if let Some(fields) = item.trap {
            ui.text_colored(
                MUTED,
                format!(
                    "    Trap: 0xA4 рамка {} | 0x94 размер текстуры {}",
                    format_rect(fields.bounding),
                    format_rect(fields.texture_size),
                ),
            );
        }
        // Координаты подвижной ловушки: по ним видно, какое из полей
        // действительно едет вслед за ней, а какое стоит на месте спавна.
        if let Some(boom) = item.boom {
            ui.text_colored(
                MUTED,
                format!(
                    "    Boom: 0xB0 старт {} | 0xC8 пред. {} | 0xD0 симуляция {}",
                    format_point(boom.original_position),
                    format_point(boom.previous_position),
                    format_point(boom.simulation_position),
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
///
/// Два знака после запятой, а не ноль: координаты в единицах симуляции —
/// это единицы и десятые доли, и округление до целых прятало бы как раз то
/// движение, ради которого их и смотрят.
fn format_point(point: Option<(f32, f32)>) -> String {
    match point {
        Some((x, y)) => format!("{x:.2},{y:.2}"),
        None => "-".to_string(),
    }
}

/// Живые флаги приседания: читаются и правятся прямо в памяти.
///
/// Нужны, потому что из дампа видно имена полей, но не то, какое из них игра
/// проверяет перед тем, как не дать шагнуть. Строка с `m_Movement` рядом
/// показывает, доходит ли до персонажа само перемещение.
///
/// Свободная функция, а не метод: вызывается там, где настройки игрока уже
/// взяты в изменяемое заимствование, и `&self` конфликтовал бы с ним.
fn draw_crouch_flags(ui: &Ui, addr: usize) {
    ui.text_colored(MUTED, "Присед:");
    for (label, offset, tooltip) in game::CROUCH_FLAGS {
        ui.same_line();
        let Some(mut value) = game::player_flag(addr, offset) else {
            ui.text_colored(MUTED, "-");
            continue;
        };
        if ui.checkbox(label, &mut value) {
            game::set_player_flag(addr, offset, value);
            log::info!("флаг {label} (0x{offset:X}) = {value}");
        }
        if ui.is_item_hovered() {
            ui.tooltip_text(format!(
                "0x{offset:X}
{tooltip}"
            ));
        }
    }
    ui.same_line();
    match game::move_speed(addr) {
        Some(speed) => ui.text_colored(MUTED, format!("moveSpeed = {speed:.2}")),
        None => ui.text_colored(MUTED, "moveSpeed = ?"),
    }
}

/// Куда игрок просит идти — по тем же клавишам, что читает игра.
///
/// Обе нажатые сразу дают ноль: в игре они складываются в противоположные
/// слагаемые и гасят друг друга, и здесь должно быть так же.
fn movement_direction(ui: &Ui) -> Option<f32> {
    if !game_has_focus() || ui.io().want_capture_keyboard {
        return None;
    }
    let left = MOVE_LEFT_KEYS.iter().copied().any(key_down);
    let right = MOVE_RIGHT_KEYS.iter().copied().any(key_down);
    match (left, right) {
        (true, false) => Some(-1.0),
        (false, true) => Some(1.0),
        _ => None,
    }
}

/// Флаг, общий для целой группы объектов.
///
/// Показывает состояние первого, пишет во все. Если объекты группы разошлись,
/// к метке добавляется звёздочка: клик тогда не переключает флаг, а выравнивает
/// группу по показанному значению. Идентификатор виджета от звёздочки не
/// зависит — он задан частью после `##`, иначе ImGui терял бы состояние
/// ровно в тот момент, когда группа расходится.
fn group_flag(ui: &Ui, label: &str, tooltip: &str, addresses: &[usize], offset: usize) {
    let mut values = addresses
        .iter()
        .map(|&addr| game::trap_flag(addr, offset))
        .peekable();
    let Some(Some(mut value)) = values.peek().copied() else {
        ui.text_colored(MUTED, "-");
        return;
    };
    let mixed = values.any(|other| other != Some(value));

    let caption = if mixed {
        format!("{label}*##{label}")
    } else {
        format!("{label}##{label}")
    };
    if ui.checkbox(caption, &mut value) {
        for &addr in addresses {
            game::set_trap_flag(addr, offset, value);
        }
    }
    if ui.is_item_hovered() {
        ui.tooltip_text(if mixed {
            format!(
                "{tooltip}
Значения в группе разошлись — клик выровняет все {}",
                addresses.len()
            )
        } else {
            format!(
                "{tooltip}
Пишется во все объекты класса ({})",
                addresses.len()
            )
        });
    }
}

/// Сырые поля объекта за общей частью `WorldObject`.
///
/// Раскладки новых типов (`Spinner`, `Trampoline`, `TrapBlock`, `Weather`)
/// у нас не сняты, а гадать смещения и писать по ним нельзя. Зато прочитать
/// и показать — можно: по столбцу чисел видно, где скорость вращения, где
/// счётчик, а где указатель. Только чтение, ничего не пишется.
fn draw_raw_fields(ui: &Ui, addr: usize) {
    /// Общая часть заканчивается здесь — дальше начинается своё у каждого типа.
    const FROM: usize = 0x90;
    /// Дальше этого заглядывать смысла нет: самый длинный известный класс,
    /// `BoomTrap`, укладывается в 0xE0.
    const TO: usize = 0x100;

    for offset in (FROM..TO).step_by(4) {
        let Some(raw) = crate::mem::read::<u32>(addr + offset) else {
            ui.text_colored(ALERT, format!("    +0x{offset:02X} недоступно"));
            return;
        };
        let float = f32::from_bits(raw);
        // Дробное показываем, только когда оно осмысленно: биты указателя,
        // прочитанные как float, дают числа вроде 1e-38 и только мешают.
        let as_float = if float.is_finite() && (float == 0.0 || float.abs() > 1e-3) {
            format!(" f={float:.3}")
        } else {
            String::new()
        };
        ui.text_colored(
            MUTED,
            format!(
                "    +0x{offset:02X}  0x{raw:08X}  i={}{as_float}",
                raw as i32
            ),
        );
    }
}

/// Форматирует одиночное число для диагностики.
fn format_scalar(value: Option<f32>) -> String {
    match value {
        Some(value) => format!("{value:.2}"),
        None => "-".to_string(),
    }
}

fn display_or_dash(value: &str) -> &str {
    if value.is_empty() { "—" } else { value }
}

/// Куда пишется лог. Показывается всегда: пустой лог и лог, который не
/// открылся, — разные диагнозы, и путать их не должно быть возможности.
fn draw_log_location(ui: &Ui) {
    let destination = log::status();
    match (&destination.path, &destination.problem) {
        (Some(path), _) => ui.text_colored(MUTED, format!("Лог: {}", path.display())),
        (None, problem) => {
            ui.text_colored(ALERT, "Файл лога не открылся, пишем только в отладчик");
            if ui.is_item_hovered() {
                ui.tooltip_text(problem.as_deref().unwrap_or("причина не записана"));
            }
        }
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
