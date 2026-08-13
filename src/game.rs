//! Разбор игровых объектов поверх [`crate::mem`] и [`crate::offsets`].
//!
//! # Опознание объектов
//!
//! Хук приносит указатели на всё, что двигалось в этом кадре, а не только на
//! игроков. Прежняя версия принимала любой указатель, попавший в диапазон
//! `0x10000..0x7FFFFFFF`, и разыменовывала его — отсюда и рамки ESP вокруг
//! случайных объектов, и вылеты.
//!
//! Здесь объект считается игроком, только если указатель на его таблицу
//! методов совпадает с эталонным. Эталон добывается один раз (см.
//! [`bootstrap`]) по самопроверяющемуся признаку: объект объявляет себя
//! локальным игроком **и** находит сам себя в списке игроков своего
//! `GameplayScreen`. Случайный мусор такую проверку не пройдёт.
//!
//! Список игроков и ловушек строится заново каждый кадр: сборщик мусора .NET
//! уплотняет кучу и двигает объекты, так что вчерашний адрес сегодня ничего
//! не значит. Между кадрами переживает только подсказка [`LAST_LOCAL`], и та
//! проходит полную проверку перед каждым использованием.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::mem;
use crate::offsets::{
    METHOD_TABLE, MIN_VALID_ADDRESS, PTR_SIZE, camera as camera_fields, list, player, screen, trap,
};

/// Верхняя граница разумного размера рамки игрока в игровых единицах.
const MAX_DIMENSION: i32 = 2000;

/// Для ловушек предел заметно выше: элементы уровня бывают куда крупнее
/// персонажа, а общий предел в 2000 мог отбраковывать их целиком — что
/// выглядело бы как «ESP ловушек не рисуется».
const MAX_TRAP_DIMENSION: i32 = 10_000;

/// Больше игроков в сессии не бывает; значение сверх этого означает, что мы
/// читаем не список игроков.
const MAX_PLAYERS: usize = 16;

/// Верхняя граница числа ловушек на уровне.
pub const MAX_TRAPS: usize = 1024;

/// Таблица методов класса `Player`, определённая при первом успешном разборе.
/// Ноль означает «ещё не знаем».
static PLAYER_CLASS: AtomicUsize = AtomicUsize::new(0);

/// Адрес локального игрока с прошлого кадра. Подсказка, а не источник
/// истины: перед использованием проверяется целиком (см. [`find_local`]).
static LAST_LOCAL: AtomicUsize = AtomicUsize::new(0);

// ============================================================================
// ПРИМИТИВЫ
// ============================================================================

/// Прямоугольник XNA (`Rectangle`): четыре подряд лежащих `i32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    fn read(addr: usize) -> Option<Self> {
        let [x, y, w, h] = mem::read::<[i32; 4]>(addr)?;
        Some(Self { x, y, w, h })
    }

    /// Похоже ли это на настоящую рамку.
    ///
    /// Условия на `x` и `y` намеренно отсутствуют: прежняя версия требовала
    /// `x != 0 && y != 0` и потому не рисовала объекты, стоящие на нулевой
    /// координате.
    pub fn is_plausible(&self) -> bool {
        self.is_plausible_within(MAX_DIMENSION)
    }

    /// То же, но с собственным пределом размера.
    pub fn is_plausible_within(&self, max_dimension: i32) -> bool {
        self.w > 0 && self.h > 0 && self.w <= max_dimension && self.h <= max_dimension
    }
}

/// Рамка в мировых координатах.
///
/// Вещественная, а не целая, как [`Rect`]: у подвижных объектов положение
/// задаётся парой `f32`, и округление до целых заставляло бы рамку дёргаться.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl From<Rect> for RectF {
    fn from(rect: Rect) -> Self {
        Self {
            x: rect.x as f32,
            y: rect.y as f32,
            w: rect.w as f32,
            h: rect.h as f32,
        }
    }
}

/// Распаковывает `Microsoft.Xna.Framework.Color`.
///
/// Упаковка — `R | G << 8 | B << 16 | A << 24`, то есть RGBA, а не ARGB, как
/// утверждал комментарий в прежней версии (сам код при этом был верен).
pub fn unpack_color(packed: u32) -> [f32; 4] {
    [
        (packed & 0xFF) as f32 / 255.0,
        ((packed >> 8) & 0xFF) as f32 / 255.0,
        ((packed >> 16) & 0xFF) as f32 / 255.0,
        ((packed >> 24) & 0xFF) as f32 / 255.0,
    ]
}

/// Читает `List<T>` и возвращает адреса его элементов.
fn read_list(list_addr: usize, max_len: usize) -> Vec<usize> {
    let Some(items) = mem::read_ptr(list_addr + list::ITEMS) else {
        return Vec::new();
    };
    let Some(size) = mem::read::<i32>(list_addr + list::SIZE) else {
        return Vec::new();
    };
    if size <= 0 {
        return Vec::new();
    }

    let size = (size as usize).min(max_len);
    let mut out = Vec::with_capacity(size);
    for index in 0..size {
        let slot = items + list::ARRAY_DATA + index * PTR_SIZE;
        if let Some(element) = mem::read_ptr(slot)
            && element >= MIN_VALID_ADDRESS
        {
            out.push(element);
        }
    }
    out
}

// ============================================================================
// ИГРОКИ
// ============================================================================

/// Снимок полей игрока, снятый за один кадр.
#[derive(Clone, Debug)]
pub struct PlayerView {
    pub addr: usize,
    /// `m_UniqueID` — ключ пользовательских настроек.
    pub unique_id: i32,
    pub name: String,
    pub color: [f32; 4],
    pub position: (f32, f32),
    pub rect: Option<Rect>,
    pub is_local: bool,
}

/// Состояние мира на текущий кадр.
#[derive(Default)]
pub struct World {
    /// `GameplayScreen` локального игрока; 0, если он не найден.
    pub screen: usize,
    /// Все игроки, локальный — первым.
    pub players: Vec<PlayerView>,
}

/// Известна ли уже таблица методов класса `Player`.
pub fn player_class() -> Option<usize> {
    match PLAYER_CLASS.load(Ordering::Relaxed) {
        0 => None,
        class => Some(class),
    }
}

/// Является ли адрес живым объектом класса `Player`.
fn is_player(addr: usize) -> bool {
    let Some(class) = player_class() else {
        return false;
    };
    mem::is_readable(addr, player::PROBE_SIZE) && mem::read_ptr(addr + METHOD_TABLE) == Some(class)
}

/// Наш ли это персонаж.
///
/// Двух признаков недостаточно поодиночке: в сетевой игре наш игрок помечен
/// `isLocal == 1`, но в одиночной сеть не участвует и флаг остаётся нулём.
/// `RemotePlayer` же равен нулю у нашего персонажа в обоих режимах и единице
/// у чужого, так что вместе они дают верный ответ во всех трёх случаях.
fn is_local(addr: usize) -> bool {
    mem::read::<u8>(addr + player::IS_LOCAL) == Some(1)
        || mem::read::<u8>(addr + player::REMOTE_PLAYER) == Some(0)
}

/// Признаки игрока, проверяемые не выходя за пределы самого объекта.
///
/// Прежняя версия требовала вдобавок, чтобы кандидат нашёлся в списке игроков
/// своего `GameplayScreen`. Проверка выглядела убедительно, но опиралась на
/// раскладку экрана из дампа **онлайновой** сессии. В одиночной игре класс
/// экрана другой, `screen + 0x100` списком игроков не является, цепочка
/// рвалась — и класс не определялся никогда. А без класса переставали
/// работать разом и список игроков, и ESP.
fn looks_like_player(addr: usize) -> bool {
    if !mem::is_readable(addr, player::PROBE_SIZE) {
        return false;
    }
    // isLocal — булев флаг; любое другое значение означает, что по этому
    // адресу лежит не игрок.
    if !matches!(mem::read::<u8>(addr + player::IS_LOCAL), Some(0 | 1)) {
        return false;
    }
    // GameplayScreen — обязательная ссылка на другой managed-объект.
    let Some(screen_addr) = mem::read_ptr(addr + player::GAMEPLAY_SCREEN) else {
        return false;
    };
    if !mem::is_readable(screen_addr, PTR_SIZE) {
        return false;
    }
    // m_BoundingRect обязан выглядеть настоящей рамкой.
    Rect::read(addr + player::BOUNDING_RECT).is_some_and(|rect| rect.is_plausible())
}

/// Все известные списки игроков экрана, объединённые и без дубликатов.
///
/// Списков три, потому что в разных режимах заполнены разные: в одиночной
/// игре — `PlayerList`, в сетевой к нему добавляются `RemotePlayerList` и
/// `MergedPlayerList`. Все три принадлежат базовому классу экрана, так что
/// читать их безопасно в любом режиме — незаполненный просто даст пусто.
fn player_lists(screen_addr: usize) -> Vec<usize> {
    const LISTS: [usize; 3] = [
        screen::PLAYER_LIST,
        screen::REMOTE_PLAYER_LIST,
        screen::MERGED_PLAYER_LIST,
    ];

    let mut addresses = Vec::new();
    for offset in LISTS {
        if let Some(list_addr) = mem::read_ptr(screen_addr + offset) {
            addresses.extend(read_list(list_addr, MAX_PLAYERS));
        }
    }
    addresses.sort_unstable();
    addresses.dedup();
    addresses
}

/// Определяет таблицу методов класса `Player`.
///
/// Признак самопроверяющийся: кандидат обязан найти сам себя в списке игроков
/// своего же `GameplayScreen`. Случайный мусор такую замкнутую ссылку не даст.
///
/// Списки читаются по смещениям **базового** класса экрана, поэтому проверка
/// работает и в одиночной игре, и в сетевой. Требования «объект объявил себя
/// локальным» здесь намеренно нет: в одиночной игре `isLocal` остаётся нулём,
/// и именно на этом опознание класса раньше и застревало.
fn bootstrap(objects: &[usize]) {
    for &addr in objects {
        if !looks_like_player(addr) {
            continue;
        }
        let Some(screen_addr) = mem::read_ptr(addr + player::GAMEPLAY_SCREEN) else {
            continue;
        };
        if !player_lists(screen_addr).contains(&addr) {
            continue;
        }
        let Some(class) = mem::read_ptr(addr + METHOD_TABLE) else {
            continue;
        };
        if class < MIN_VALID_ADDRESS {
            continue;
        }

        PLAYER_CLASS.store(class, Ordering::Relaxed);
        crate::log::info!("класс Player опознан: таблица методов 0x{class:X}");
        return;
    }
}

/// Ищет локального игрока.
///
/// Хук приносит только те объекты, для которых в этом кадре вызвался
/// `SetPosition`, — стоящий на месте игрок туда не попадает. Поэтому сначала
/// пробуется адрес с прошлого кадра, и только затем буфер хука.
///
/// Подсказка не принимается на веру: [`is_player`] заново проверяет
/// доступность памяти и совпадение таблицы методов, так что переехавший
/// после сборки мусора объект будет отвергнут. Чужой объект по этому адресу
/// проверку не пройдёт, а если её пройдёт другой экземпляр `Player`,
/// объявляющий себя локальным, — это и есть тот, кто нам нужен.
fn find_local(objects: &[usize]) -> Option<usize> {
    let cached = LAST_LOCAL.load(Ordering::Relaxed);
    if cached != 0 && is_player(cached) && is_local(cached) {
        return Some(cached);
    }

    let found = objects
        .iter()
        .copied()
        .find(|&addr| is_player(addr) && is_local(addr));
    LAST_LOCAL.store(found.unwrap_or(0), Ordering::Relaxed);
    found
}

/// Сбрасывает подсказки, накопленные между кадрами.
pub fn forget_cached_pointers() {
    LAST_LOCAL.store(0, Ordering::Relaxed);
}

fn read_player(addr: usize) -> Option<PlayerView> {
    let [x, y] = mem::read::<[f32; 2]>(addr + player::POSITION)?;
    Some(PlayerView {
        addr,
        unique_id: mem::read::<i32>(addr + player::UNIQUE_ID)?,
        name: mem::read_ptr(addr + player::NAME)
            .and_then(mem::read_dotnet_string)
            .unwrap_or_default(),
        color: unpack_color(mem::read::<u32>(addr + player::COLOR)?),
        position: (x, y),
        rect: Rect::read(addr + player::BOUNDING_RECT).filter(Rect::is_plausible),
        is_local: is_local(addr),
    })
}

/// Отбирает игроков из списка адресов: отсеивает чужие объекты, убирает
/// дубликаты (хук приносит один объект по нескольку раз за кадр) и ставит
/// локального первым — на этот порядок опирается интерфейс.
fn collect_players(addresses: &[usize]) -> Vec<PlayerView> {
    let mut unique: Vec<usize> = addresses
        .iter()
        .copied()
        .filter(|&addr| is_player(addr))
        .collect();
    unique.sort_unstable();
    unique.dedup();

    let mut players: Vec<PlayerView> = unique.into_iter().filter_map(read_player).collect();
    players.sort_by_key(|player| if player.is_local { 0 } else { 1 });
    players
}

/// Строит снимок мира из указателей, собранных хуком за кадр.
pub fn build_world(objects: &[usize]) -> World {
    let mut world = World::default();

    if player_class().is_none() {
        bootstrap(objects);
    }

    let Some(local_addr) = find_local(objects) else {
        return world;
    };

    // Экран нужен для списков игроков и ловушек. Проверяем его только на
    // читаемость указателя: требовать целиком `screen::PROBE_SIZE` нельзя —
    // в одиночной игре класс экрана другой и может быть короче.
    world.screen = mem::read_ptr(local_addr + player::GAMEPLAY_SCREEN)
        .filter(|&screen_addr| mem::is_readable(screen_addr, PTR_SIZE))
        .unwrap_or(0);

    // Список игроков экрана полнее буфера хука: в нём есть и те, кто в этом
    // кадре не двигался. Но его смещение снято с онлайновой сессии, поэтому
    // результат обязательно проверяется, а при неудаче берётся буфер хука —
    // именно так работала последняя версия, у которой ESP не отваливался.
    world.players = players_from_screen(world.screen);
    if world.players.is_empty() {
        world.players = collect_players(objects);
    }

    world
}

/// Читает игроков из списков экрана.
fn players_from_screen(screen_addr: usize) -> Vec<PlayerView> {
    if screen_addr == 0 {
        return Vec::new();
    }
    collect_players(&player_lists(screen_addr))
}

/// Сетевая ли сессия — по флагу `m_Online` экрана.
pub fn is_online(screen_addr: usize) -> Option<bool> {
    if screen_addr == 0 {
        return None;
    }
    mem::read::<u8>(screen_addr + screen::IS_ONLINE).map(|value| value != 0)
}

/// Указатель на объект камеры.
pub fn camera(screen_addr: usize) -> Option<usize> {
    if screen_addr == 0 {
        return None;
    }
    mem::read_ptr(screen_addr + screen::CAMERA).filter(|&addr| addr >= MIN_VALID_ADDRESS)
}

// ============================================================================
// КАМЕРА
// ============================================================================

/// Аффинное преобразование плоскости: матрица 2×2 и перенос.
///
/// XNA хранит `Matrix` построчно, а `Vector2.Transform` берёт из шестнадцати
/// её чисел ровно шесть: `M11, M12, M21, M22, M41, M42`. Остальные описывают
/// третье измерение, которого у двумерной игры нет.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub m11: f32,
    pub m12: f32,
    pub m21: f32,
    pub m22: f32,
    pub m41: f32,
    pub m42: f32,
}

impl Affine {
    /// Масштаб вокруг заданной точки — то, что раньше делали ползунки
    /// `Zoom`, `Cam X` и `Cam Y`.
    pub fn from_zoom_and_offset(zoom: f32, camera_x: f32, camera_y: f32) -> Self {
        Self {
            m11: zoom,
            m12: 0.0,
            m21: 0.0,
            m22: zoom,
            m41: -camera_x * zoom,
            m42: -camera_y * zoom,
        }
    }

    /// Читает `Matrix` XNA по адресу и оставляет от неё двумерную часть.
    fn read(addr: usize) -> Option<Self> {
        let m = mem::read::<[f32; 16]>(addr)?;
        Some(Self {
            m11: m[0],
            m12: m[1],
            m21: m[4],
            m22: m[5],
            m41: m[12],
            m42: m[13],
        })
    }

    pub fn determinant(self) -> f32 {
        self.m11 * self.m22 - self.m12 * self.m21
    }

    /// Годится ли преобразование для рисования.
    ///
    /// Вырожденную матрицу нужно отсечь до, а не после употребления: она
    /// схлопнула бы весь мир в точку, а обратное преобразование (телепорт)
    /// дало бы бесконечность.
    pub fn is_usable(self) -> bool {
        let finite = [self.m11, self.m12, self.m21, self.m22, self.m41, self.m42]
            .iter()
            .all(|value| value.is_finite());
        let scale = self.determinant().abs();
        finite && (1e-6..1e12).contains(&scale)
    }

    pub fn to_screen(self, x: f32, y: f32) -> [f32; 2] {
        [
            x * self.m11 + y * self.m21 + self.m41,
            x * self.m12 + y * self.m22 + self.m42,
        ]
    }

    /// Обратное преобразование — нужно телепорту, который получает точку
    /// в экранных координатах курсора.
    pub fn to_world(self, x: f32, y: f32) -> (f32, f32) {
        let det = self.determinant();
        if det.abs() < f32::EPSILON {
            return (x, y);
        }
        let dx = x - self.m41;
        let dy = y - self.m42;
        (
            (dx * self.m22 - dy * self.m21) / det,
            (dy * self.m11 - dx * self.m12) / det,
        )
    }

    /// Досогласовывает преобразование с фактическим размером окна.
    ///
    /// Камера считает свою матрицу под собственный вьюпорт. Если игра рисует
    /// в буфер одного размера, а показывает его в окне другого, ESP уезжает
    /// ровно на это отношение. Вписываем вьюпорт в окно с сохранением
    /// пропорций и центрированием — при совпадающих размерах это тождество,
    /// так что обычному случаю метод не мешает.
    pub fn fit_to_display(self, viewport: (f32, f32), display: (f32, f32)) -> Self {
        let sane = [viewport.0, viewport.1, display.0, display.1]
            .iter()
            .all(|value| value.is_finite() && *value > 1.0);
        if !sane {
            return self;
        }
        let scale = (display.0 / viewport.0).min(display.1 / viewport.1);
        let offset_x = (display.0 - viewport.0 * scale) * 0.5;
        let offset_y = (display.1 - viewport.1 * scale) * 0.5;
        Self {
            m11: self.m11 * scale,
            m12: self.m12 * scale,
            m21: self.m21 * scale,
            m22: self.m22 * scale,
            m41: self.m41 * scale + offset_x,
            m42: self.m42 * scale + offset_y,
        }
    }
}

/// Снимок камеры игры за один кадр.
#[derive(Clone, Copy, Debug)]
pub struct CameraView {
    pub addr: usize,
    /// Преобразование мира в экран в системе координат вьюпорта камеры.
    pub transform: Affine,
    /// Взято ли оно из `m_Transform`. Ложь означает, что матрица оказалась
    /// непригодной и преобразование собрано из зума, позиции и вьюпорта.
    pub from_matrix: bool,
    pub zoom: f32,
    pub position: (f32, f32),
    pub viewport: (f32, f32),
}

/// Читает камеру экрана.
///
/// Возвращает `None`, если камеры нет или ни матрица, ни собранное из полей
/// преобразование не выглядят пригодными: вызывающий код тогда откатывается
/// на ручные ползунки, а не рисует рамки в случайных местах.
pub fn read_camera(screen_addr: usize) -> Option<CameraView> {
    let addr = camera(screen_addr)?;
    if !mem::is_readable(addr, camera_fields::PROBE_SIZE) {
        return None;
    }

    let zoom = mem::read::<f32>(addr + camera_fields::ZOOM)?;
    let [position_x, position_y] = mem::read::<[f32; 2]>(addr + camera_fields::CAMERA_POSITION)?;
    let width = mem::read::<i32>(addr + camera_fields::VIEWPORT_WIDTH)?;
    let height = mem::read::<i32>(addr + camera_fields::VIEWPORT_HEIGHT)?;
    let viewport = (width as f32, height as f32);

    let matrix = Affine::read(addr + camera_fields::TRANSFORM).filter(|affine| affine.is_usable());
    let (transform, from_matrix) = match matrix {
        Some(affine) => (affine, true),
        // Запасной путь на случай, если матрица в этом кадре ещё не
        // пересчитана: та же формула, что игра закладывает в неё сама, —
        // сдвиг на позицию камеры, масштаб, центр вьюпорта.
        None => {
            let assembled = Affine {
                m11: zoom,
                m12: 0.0,
                m21: 0.0,
                m22: zoom,
                m41: viewport.0 * 0.5 - position_x * zoom,
                m42: viewport.1 * 0.5 - position_y * zoom,
            };
            (assembled, false)
        }
    };
    if !transform.is_usable() {
        return None;
    }

    Some(CameraView {
        addr,
        transform,
        from_matrix,
        zoom,
        position: (position_x, position_y),
        viewport,
    })
}

/// Телепортирует персонажа, записывая `m_Position`.
pub fn set_position(addr: usize, x: f32, y: f32) -> bool {
    mem::write::<[f32; 2]>(addr + player::POSITION, [x, y])
}

// ============================================================================
// ЗАПИСЬ ПАРАМЕТРОВ ИГРОКА
// ============================================================================

/// Значения параметров, как они сейчас лежат в памяти.
#[derive(Clone, Copy, Debug, Default)]
pub struct LiveStats {
    pub speed: Option<f32>,
    pub jump_height: Option<f32>,
    pub gravity: Option<f32>,
}

pub fn read_live_stats(addr: usize) -> LiveStats {
    LiveStats {
        speed: mem::read::<f32>(addr + player::DELTA_SPEED),
        jump_height: mem::read::<f32>(addr + player::INITIAL_JUMP_HEIGHT),
        gravity: mem::read::<f32>(addr + player::GRAVITY),
    }
}

/// `canDie = 0`.
pub fn set_god_mode(addr: usize) -> bool {
    mem::write::<u8>(addr + player::CAN_DIE, 0)
}

/// `CanJump = 1`, `JumpBeenReleased = 1`.
pub fn set_infinite_jump(addr: usize) -> bool {
    mem::write::<u8>(addr + player::CAN_JUMP, 1)
        && mem::write::<u8>(addr + player::JUMP_BEEN_RELEASED, 1)
}

/// `Alive = 0` — игра заметит это и заспавнит игрока заново.
pub fn respawn(addr: usize) -> bool {
    mem::write::<u8>(addr + player::ALIVE, 0)
}

pub fn set_speed(addr: usize, value: f32) -> bool {
    mem::write::<f32>(addr + player::DELTA_SPEED, value)
}

pub fn set_jump_height(addr: usize, value: f32) -> bool {
    mem::write::<f32>(addr + player::INITIAL_JUMP_HEIGHT, value)
}

/// Гравитация задаётся двумя полями: текущим и значением по умолчанию,
/// иначе игра вернёт своё при следующем респавне.
pub fn set_gravity(addr: usize, value: f32) -> bool {
    mem::write::<f32>(addr + player::GRAVITY, value)
        && mem::write::<f32>(addr + player::GRAVITY_DEFAULT, value)
}

// ============================================================================
// ЛОВУШКИ
// ============================================================================

/// Ссылка на ловушку без чтения строк: имена дороги, а нужны лишь для
/// строк, реально показанных в интерфейсе.
#[derive(Clone, Copy, Debug)]
pub struct TrapRef {
    pub addr: usize,
    /// Таблица методов — идентификатор конкретного класса ловушки.
    pub class: usize,
    /// Рамка в мировых координатах, выбранная для ESP. См. [`world_rect`].
    pub rect: Option<RectF>,
    /// `m_Bounding` (0x50) как есть, без отбраковки.
    pub bounding_raw: Option<Rect>,
    /// `m_Rectangle` (0x70) как есть — кадр в атласе, не положение в мире.
    pub source_raw: Option<Rect>,
    /// `PositionX`/`PositionY` (0x24).
    pub position: Option<(f32, f32)>,
    /// `Position` (0x88) — вторая мировая позиция.
    pub position_alt: Option<(f32, f32)>,
}

/// Читает пару подряд лежащих `f32`, отбрасывая нечисла.
fn read_vector2(addr: usize) -> Option<(f32, f32)> {
    let [x, y] = mem::read::<[f32; 2]>(addr)?;
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

/// Выбирает мировую рамку объекта.
///
/// Порядок именно такой, потому что источники неравноценны:
///
/// 1. `m_Bounding` (0x50) — готовый прямоугольник коллизии в мировых
///    координатах. Точнее всех, но у части объектов остаётся нулевым;
/// 2. позиция (0x24) плюс размер кадра (0x70). Позицию игра заполняет у
///    всех объектов из данных уровня, а размер кадра совпадает с размером
///    объекта в мире — тайлы 48×48 и 48×96 это подтверждают;
/// 3. вторая позиция (0x88) с тем же размером — на случай, если первая у
///    этого класса не используется.
///
/// Обратите внимание, чего в списке нет: самого `m_Rectangle` как рамки.
/// Прежняя версия ставила его первым, а это координаты в атласе текстур —
/// отсюда и кучка рамок в углу экрана вместо ESP.
fn world_rect(
    bounding: Option<Rect>,
    source: Option<Rect>,
    position: Option<(f32, f32)>,
    position_alt: Option<(f32, f32)>,
) -> Option<RectF> {
    if let Some(rect) = bounding.filter(|rect| rect.is_plausible_within(MAX_TRAP_DIMENSION)) {
        return Some(rect.into());
    }

    let size = source.filter(|rect| rect.is_plausible_within(MAX_TRAP_DIMENSION))?;
    let (x, y) = position
        .filter(|&(x, y)| x != 0.0 || y != 0.0)
        .or(position_alt)?;
    Some(RectF {
        x,
        y,
        w: size.w as f32,
        h: size.h as f32,
    })
}

/// Собирает ловушки уровня.
pub fn collect_traps(screen_addr: usize, out: &mut Vec<TrapRef>) {
    out.clear();
    let Some(list_addr) = mem::read_ptr(screen_addr + screen::TRAP_LIST) else {
        return;
    };

    for addr in read_list(list_addr, MAX_TRAPS) {
        if !mem::is_readable(addr, trap::PROBE_SIZE) {
            continue;
        }
        let Some(class) = mem::read_ptr(addr + METHOD_TABLE) else {
            continue;
        };
        // Все источники сохраняются сырыми: интерфейс показывает их по
        // галочке «Показать рамки», и по ним видно, откуда взялась рамка.
        let bounding_raw = Rect::read(addr + trap::BOUNDING);
        let source_raw = Rect::read(addr + trap::SOURCE_RECT);
        let position = read_vector2(addr + trap::POSITION_X);
        let position_alt = read_vector2(addr + trap::POSITION);
        out.push(TrapRef {
            addr,
            class,
            rect: world_rect(bounding_raw, source_raw, position, position_alt),
            bounding_raw,
            source_raw,
            position,
            position_alt,
        });
    }
}

pub fn trap_name(addr: usize) -> Option<String> {
    mem::read_ptr(addr + trap::NAME).and_then(mem::read_dotnet_string)
}

/// Читает булево поле ловушки.
pub fn trap_flag(addr: usize, offset: usize) -> Option<bool> {
    mem::read::<u8>(addr + offset).map(|value| value != 0)
}

/// Пишет булево поле ловушки.
pub fn set_trap_flag(addr: usize, offset: usize, value: bool) -> bool {
    mem::write::<u8>(addr + offset, u8::from(value))
}

/// Может ли объект нести поля `BoomTrap` (`Speed`, `m_CanTrigger`).
///
/// Проверяется только доступность памяти: принадлежность класса решает
/// пользователь, помечая его в интерфейсе. Автоматически определить её
/// нельзя — имена классов в рантайме нам недоступны, — а писать эти поля
/// вслепую нельзя тем более: у базового `Trap` по тем же адресам лежит
/// прямоугольник коллизии.
pub fn supports_boom_fields(addr: usize) -> bool {
    mem::is_readable(addr, trap::BOOM_PROBE_SIZE)
}

pub fn trap_speed(addr: usize) -> Option<f32> {
    mem::read::<f32>(addr + trap::BOOM_SPEED)
}

pub fn set_trap_speed(addr: usize, value: f32) -> bool {
    mem::write::<f32>(addr + trap::BOOM_SPEED, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpacks_xna_color_as_rgba() {
        // 0xAABBGGRR: A=0x80, B=0x40, G=0x20, R=0x10
        let [r, g, b, a] = unpack_color(0x8040_2010);
        assert_eq!(r, 0x10 as f32 / 255.0);
        assert_eq!(g, 0x20 as f32 / 255.0);
        assert_eq!(b, 0x40 as f32 / 255.0);
        assert_eq!(a, 0x80 as f32 / 255.0);
    }

    #[test]
    fn opaque_white_round_trips() {
        assert_eq!(unpack_color(0xFFFF_FFFF), [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(unpack_color(0x0000_0000), [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn rect_at_origin_is_plausible() {
        // Регрессия: прежняя проверка требовала x != 0 && y != 0 и
        // отбрасывала объекты на нулевой координате.
        let rect = Rect {
            x: 0,
            y: 0,
            w: 32,
            h: 32,
        };
        assert!(rect.is_plausible());
    }

    #[test]
    fn trap_limit_admits_rects_the_player_limit_rejects() {
        // Крупный элемент уровня: по игроцкому пределу отбраковывается,
        // по ловушечному — проходит.
        let rect = Rect {
            x: 0,
            y: 0,
            w: 4096,
            h: 64,
        };
        assert!(!rect.is_plausible());
        assert!(rect.is_plausible_within(MAX_TRAP_DIMENSION));
    }

    /// Числа сняты с живой игры: строка `Object_World5` в списке ловушек.
    #[test]
    fn world_bounding_wins_over_the_texture_frame() {
        let bounding = Rect {
            x: 144,
            y: 576,
            w: 48,
            h: 48,
        };
        // m_Rectangle этого же объекта: кадр в атласе, y всегда 0.
        let source = Rect {
            x: 192,
            y: 0,
            w: 48,
            h: 48,
        };
        let rect = world_rect(Some(bounding), Some(source), Some((144.0, 576.0)), None);
        assert_eq!(
            rect,
            Some(RectF {
                x: 144.0,
                y: 576.0,
                w: 48.0,
                h: 48.0
            })
        );
    }

    /// Числа сняты с живой игры: строка `Traps_SemiMedium2`, у которой
    /// `m_Bounding` пуст. Рамку приходится собирать из позиции и размера
    /// кадра — и ни в коем случае не из координат кадра.
    #[test]
    fn empty_bounding_falls_back_to_position_and_frame_size() {
        let empty = Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        let source = Rect {
            x: 48,
            y: 0,
            w: 48,
            h: 96,
        };
        let rect = world_rect(Some(empty), Some(source), Some((816.0, 480.0)), None);
        assert_eq!(
            rect,
            Some(RectF {
                x: 816.0,
                y: 480.0,
                w: 48.0,
                h: 96.0
            })
        );
    }

    #[test]
    fn zero_position_defers_to_the_second_one() {
        let source = Rect {
            x: 0,
            y: 0,
            w: 48,
            h: 48,
        };
        let rect = world_rect(None, Some(source), Some((0.0, 0.0)), Some((240.0, 96.0)));
        assert_eq!(rect.map(|rect| (rect.x, rect.y)), Some((240.0, 96.0)));
    }

    #[test]
    fn without_any_position_there_is_no_frame() {
        let source = Rect {
            x: 96,
            y: 0,
            w: 48,
            h: 48,
        };
        assert_eq!(world_rect(None, Some(source), None, None), None);
    }

    #[test]
    fn camera_transform_round_trips() {
        // Матрица камеры при зуме 0.8333 и центре вьюпорта 1280x720.
        let transform = Affine {
            m11: 0.8333,
            m12: 0.0,
            m21: 0.0,
            m22: 0.8333,
            m41: 640.0 - 400.0 * 0.8333,
            m42: 360.0 - 300.0 * 0.8333,
        };
        assert!(transform.is_usable());
        // Позиция камеры обязана оказаться ровно в центре экрана.
        assert_eq!(transform.to_screen(400.0, 300.0), [640.0, 360.0]);

        let (x, y) = transform.to_world(640.0, 360.0);
        assert!((x - 400.0).abs() < 0.01 && (y - 300.0).abs() < 0.01);
    }

    #[test]
    fn degenerate_transforms_are_rejected() {
        let zero = Affine::from_zoom_and_offset(0.0, 0.0, 0.0);
        assert!(!zero.is_usable());

        let nan = Affine {
            m11: f32::NAN,
            ..Affine::from_zoom_and_offset(1.0, 0.0, 0.0)
        };
        assert!(!nan.is_usable());
    }

    #[test]
    fn fitting_to_an_equal_display_changes_nothing() {
        let transform = Affine::from_zoom_and_offset(0.8333, 120.0, -45.0);
        assert_eq!(
            transform.fit_to_display((1280.0, 720.0), (1280.0, 720.0)),
            transform
        );
    }

    #[test]
    fn fitting_to_a_doubled_display_doubles_the_scale() {
        let transform = Affine::from_zoom_and_offset(1.0, 0.0, 0.0);
        let fitted = transform.fit_to_display((1280.0, 720.0), (2560.0, 1440.0));
        assert_eq!(fitted.to_screen(100.0, 50.0), [200.0, 100.0]);
    }

    #[test]
    fn degenerate_and_oversized_rects_are_rejected() {
        assert!(
            !Rect {
                x: 5,
                y: 5,
                w: 0,
                h: 32
            }
            .is_plausible()
        );
        assert!(
            !Rect {
                x: 5,
                y: 5,
                w: 32,
                h: 0
            }
            .is_plausible()
        );
        assert!(
            !Rect {
                x: 5,
                y: 5,
                w: -32,
                h: 32
            }
            .is_plausible()
        );
        assert!(
            !Rect {
                x: 5,
                y: 5,
                w: MAX_DIMENSION + 1,
                h: 32
            }
            .is_plausible()
        );
    }
}
