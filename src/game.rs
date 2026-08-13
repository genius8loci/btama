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

use std::collections::HashMap;
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

/// Флаги, вокруг которых крутится запрет ходить в приседе.
///
/// Какой именно из них игра проверяет, по дампу не видно: у нас есть имена
/// полей, но не код метода `Update`. Поэтому все они показываются живыми
/// значениями и правятся вручную — так нужный находится за один заход,
/// а не за пять пересборок.
pub const CROUCH_FLAGS: [(&str, usize); 5] = [
    ("Crouching", player::CROUCHING),
    ("ForceCrouch", player::FORCE_CROUCH),
    ("PlayerMove", player::PLAYER_MOVE),
    ("allowInput", player::ALLOW_INPUT),
    ("MoveAnyhow", player::MOVE_ANYHOW),
];

pub fn player_flag(addr: usize, offset: usize) -> Option<bool> {
    mem::read::<u8>(addr + offset).map(|value| value != 0)
}

pub fn set_player_flag(addr: usize, offset: usize, value: bool) -> bool {
    mem::write::<u8>(addr + offset, u8::from(value))
}

/// Пригнулся ли персонаж прямо сейчас.
pub fn is_crouching(addr: usize) -> bool {
    player_flag(addr, player::CROUCHING).unwrap_or(false)
        || player_flag(addr, player::FORCE_CROUCH).unwrap_or(false)
}

/// Снимает запрет на движение в приседе, взводя `m_MovePlayerAnyhow`.
///
/// Флаг выбран по имени — «двигать персонажа несмотря ни на что», — но это
/// догадка, а не факт: проверить её можно только в игре. Поэтому он и
/// применяется лишь пока персонаж действительно пригнулся, и лишь по явно
/// включённому переключателю.
pub fn allow_crouch_movement(addr: usize) -> bool {
    set_player_flag(addr, player::MOVE_ANYHOW, true)
}

/// `m_Movement` (0x130) — горизонтальное перемещение, посчитанное игрой.
pub fn movement(addr: usize) -> Option<f32> {
    mem::read::<f32>(addr + player::MOVEMENT)
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
    /// `Position` (0x88) — позиция в единицах симуляции.
    pub position_alt: Option<(f32, f32)>,
    /// Поля класса `Trap`; `None`, если роль класса не назначена.
    pub trap: Option<TrapFields>,
    /// Поля класса `BoomTrap`; `None`, если роль класса не назначена.
    pub boom: Option<BoomFields>,
}

/// Какие поля за 0x90 можно читать у этого класса.
///
/// Раскладки классов расходятся после 0x90, и раньше роль приходилось
/// назначать вручную — казалось, что различить типы нечем. Оказалось, что
/// игра сама подписывает объект: в `ObjectType` (0x10) лежит строка с именем
/// класса, `"BoomTrap"` или `"Trap"`. Её и берём.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrapRole {
    /// Только поля `WorldObject` (до 0x90) — безопасно для любого класса.
    #[default]
    Base,
    /// `Bloody_Trapland.WorldObjects.Trap`.
    Trap,
    /// `Bloody_Trapland.WorldObjects.BoomTrap`.
    Boom,
}

impl TrapRole {
    /// Выводит роль из строки `ObjectType`.
    ///
    /// Хвост после точки берётся на случай, если игра положит туда полное
    /// имя с пространством имён; регистр не учитывается — обе вольности
    /// ничего не стоят, а от разночтений страхуют.
    pub fn from_object_type(object_type: &str) -> Self {
        let name = object_type
            .rsplit(['.', '+'])
            .next()
            .unwrap_or(object_type)
            .trim();
        if name.eq_ignore_ascii_case("BoomTrap") {
            Self::Boom
        } else if name.eq_ignore_ascii_case("Trap") {
            Self::Trap
        } else {
            Self::Base
        }
    }
}

/// Что известно о классе объектов из списка ловушек.
///
/// Кэшируется по указателю на таблицу методов, и это надёжно: таблицы живут
/// в загрузочной куче, которую сборщик мусора не двигает, так что указатель
/// однозначно и навсегда соответствует одному типу. Строки читаются один раз
/// на класс, а не каждый кадр на каждый объект.
#[derive(Clone, Debug, Default)]
pub struct ClassInfo {
    /// `ObjectType` (0x10) — имя типа, как его хранит сама игра.
    pub object_type: String,
    /// `Name` (0x20) первого встреченного объекта этого класса.
    pub sample_name: String,
    pub role: TrapRole,
}

impl ClassInfo {
    fn read(addr: usize) -> Self {
        let object_type = read_string_field(addr, trap::OBJECT_TYPE);
        Self {
            role: TrapRole::from_object_type(&object_type),
            sample_name: read_string_field(addr, trap::NAME),
            object_type,
        }
    }

    /// Чем подписывать объект — тип, если игра его назвала, иначе имя ассета.
    pub fn label(&self) -> &str {
        if !self.object_type.is_empty() {
            &self.object_type
        } else {
            &self.sample_name
        }
    }
}

/// Поля, которые есть только у класса `Trap`.
#[derive(Clone, Copy, Debug)]
pub struct TrapFields {
    /// `m_Bounding` (0xA4) — прямоугольник самой ловушки.
    pub bounding: Option<Rect>,
    /// `TextureSize` (0x94).
    pub texture_size: Option<Rect>,
}

/// Поля, которые есть только у `BoomTrap`-подобных классов.
///
/// Читаются лишь для классов, помеченных пользователем: у базового `Trap`
/// по этим же адресам лежит прямоугольник коллизии, и принимать его за
/// координаты значило бы показывать заведомую чушь.
///
/// `Speed` сюда не входит: ползунок в интерфейсе читает и пишет его
/// напрямую, и снимок кадровой давности только мешал бы ему.
#[derive(Clone, Copy, Debug)]
pub struct BoomFields {
    /// `OriginalPosition` (0xB0).
    pub original_position: Option<(f32, f32)>,
    /// `PreviousPosition` (0xC8).
    pub previous_position: Option<(f32, f32)>,
    /// `SimulationPosition` (0xD0).
    pub simulation_position: Option<(f32, f32)>,
}

/// Читает пару подряд лежащих `f32`, отбрасывая нечисла.
fn read_vector2(addr: usize) -> Option<(f32, f32)> {
    let [x, y] = mem::read::<[f32; 2]>(addr)?;
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

/// Во сколько раз единица физического движка крупнее пикселя мира.
///
/// Игра хранит положение объекта дважды: `m_Bounding` (0x50) — в пикселях,
/// `Position` (0x88) — в единицах симуляции. Farseer по умолчанию берёт
/// 100 пикселей на единицу, и снятые с игры пары это подтверждают:
/// `144,576` против `1.44,5.76` и `1488,576` против `14.88,5.76`.
///
/// Константа всё же только запасная: масштаб выводится из самих данных
/// (см. [`derive_sim_scale`]), потому что подставлять сюда неверное число
/// значит промахнуться рамками ровно во столько же раз.
const SIM_TO_DISPLAY_DEFAULT: f32 = 100.0;

/// Правдоподобные границы выведенного масштаба. Всё за их пределами
/// означает, что мы поделили одно случайное число на другое.
const SIM_TO_DISPLAY_RANGE: std::ops::RangeInclusive<f32> = 1.0..=1000.0;

/// Выводит масштаб симуляции по объектам, у которых заполнены обе позиции.
///
/// Такие в списке есть всегда: элементы самого уровня (`Object_World*`)
/// несут и `m_Bounding` в пикселях, и `Position` в единицах движка.
fn derive_sim_scale(traps: &[TrapRef]) -> f32 {
    for item in traps {
        let Some(bounding) = item
            .bounding_raw
            .filter(|rect| rect.is_plausible_within(MAX_TRAP_DIMENSION))
        else {
            continue;
        };
        let Some((sim_x, sim_y)) = item.position_alt else {
            continue;
        };
        // Берём ту ось, где число крупнее: у координаты, близкой к нулю,
        // относительная погрешность выше.
        let (display, sim) = if sim_x.abs() >= sim_y.abs() {
            (bounding.x as f32, sim_x)
        } else {
            (bounding.y as f32, sim_y)
        };
        if sim.abs() < 0.01 {
            continue;
        }
        let scale = display / sim;
        if SIM_TO_DISPLAY_RANGE.contains(&scale) {
            return scale;
        }
    }
    SIM_TO_DISPLAY_DEFAULT
}

/// Выбирает мировую рамку объекта.
///
/// Порядок именно такой, потому что источники неравноценны:
///
/// 1. `m_Bounding` (0x50) — готовый прямоугольник коллизии прямо в пикселях
///    мира. Точнее всех, но заполнен только у элементов уровня; у собственно
///    ловушек остаётся нулевым;
/// 2. `Trap.m_Bounding` (0xA4) — такой же прямоугольник, но объявленный уже
///    самой ловушкой. Только при роли [`TrapRole::Trap`]: у остальных классов
///    по этому адресу либо другое поле, либо вообще конец объекта;
/// 3. `Position` (0x88), переведённая из единиц симуляции, плюс размер кадра
///    (0x70). Размер кадра совпадает с размером объекта в мире — тайлы 48×48
///    и 48×96 это подтверждают;
/// 4. `PositionX`/`PositionY` (0x24) с тем же размером. В снятой сборке эта
///    пара везде нулевая, но она есть в раскладке и ничего не стоит.
///
/// Обратите внимание, чего в списке нет: самого `m_Rectangle` как рамки.
/// Прежняя версия ставила его первым, а это координаты в атласе текстур —
/// отсюда и рамки кучей в углу экрана вместо ESP.
///
/// Нулевую позицию мы пропускаем. Объект ровно в начале координат из-за
/// этого рамку потеряет, но такой объект и так несёт `m_Bounding` и будет
/// пойман первым правилом, а вот незаполненное поле нулём притворяется
/// постоянно — и именно оно собирало рамки в левом верхнем углу.
fn world_rect(item: &TrapRef, sim_scale: f32) -> Option<RectF> {
    let plausible = |rect: &Rect| rect.is_plausible_within(MAX_TRAP_DIMENSION);

    if let Some(rect) = item
        .bounding_raw
        .filter(plausible)
        .or_else(|| item.trap.and_then(|trap| trap.bounding).filter(plausible))
    {
        return Some(rect.into());
    }

    let size = item.source_raw.filter(plausible)?;
    let non_zero = |&(x, y): &(f32, f32)| x != 0.0 || y != 0.0;
    let (x, y) = match item.position_alt.filter(non_zero) {
        Some((sim_x, sim_y)) => (sim_x * sim_scale, sim_y * sim_scale),
        None => item.position.filter(non_zero)?,
    };
    Some(RectF {
        x,
        y,
        w: size.w as f32,
        h: size.h as f32,
    })
}

/// Читает поля класса `Trap`.
fn read_trap_fields(addr: usize) -> Option<TrapFields> {
    if !mem::is_readable(addr, trap::TRAP_PROBE_SIZE) {
        return None;
    }
    Some(TrapFields {
        bounding: Rect::read(addr + trap::TRAP_BOUNDING),
        texture_size: Rect::read(addr + trap::TRAP_TEXTURE_SIZE),
    })
}

/// Читает поля класса `BoomTrap`.
fn read_boom_fields(addr: usize) -> Option<BoomFields> {
    if !mem::is_readable(addr, trap::BOOM_PROBE_SIZE) {
        return None;
    }
    Some(BoomFields {
        original_position: read_vector2(addr + trap::BOOM_ORIGINAL_POSITION),
        previous_position: read_vector2(addr + trap::BOOM_PREVIOUS_POSITION),
        simulation_position: read_vector2(addr + trap::BOOM_SIMULATION_POSITION),
    })
}

/// Собирает ловушки уровня и возвращает применённый масштаб симуляции.
///
/// `classes` — кэш сведений о классах; незнакомые дочитываются на месте.
/// Роль решает, что можно читать за 0x90: у `SPSpawn` объект заканчивается
/// на 0x98, у `QuickGoal` — чуть дальше, и чтение прямоугольника по 0xA4 у
/// них залезло бы в соседний объект кучи. Оттуда охотно возвращается
/// правдоподобная на вид рамка — так на экране и появлялись пустые квадраты
/// в воздухе.
///
/// Проход по списку двойной: масштаб симуляции выводится из тех объектов, у
/// которых заполнены обе позиции, а применять его нужно ко всем — включая
/// те, что встретились раньше донора.
pub fn collect_traps(
    screen_addr: usize,
    classes: &mut HashMap<usize, ClassInfo>,
    out: &mut Vec<TrapRef>,
) -> f32 {
    out.clear();
    let Some(list_addr) = mem::read_ptr(screen_addr + screen::TRAP_LIST) else {
        return SIM_TO_DISPLAY_DEFAULT;
    };

    for addr in read_list(list_addr, MAX_TRAPS) {
        if !mem::is_readable(addr, trap::PROBE_SIZE) {
            continue;
        }
        let Some(class) = mem::read_ptr(addr + METHOD_TABLE) else {
            continue;
        };
        let role = classes
            .entry(class)
            .or_insert_with(|| {
                let info = ClassInfo::read(addr);
                crate::log::info!(
                    "класс 0x{class:X}: ObjectType={:?}, пример {:?} — читаем как {:?}",
                    info.object_type,
                    info.sample_name,
                    info.role
                );
                info
            })
            .role;
        // Все источники сохраняются сырыми: интерфейс показывает их по
        // галочке «Показать рамки», и по ним видно, откуда взялась рамка.
        out.push(TrapRef {
            addr,
            class,
            rect: None,
            bounding_raw: Rect::read(addr + trap::BOUNDING),
            source_raw: Rect::read(addr + trap::SOURCE_RECT),
            position: read_vector2(addr + trap::POSITION_X),
            position_alt: read_vector2(addr + trap::POSITION),
            trap: (role == TrapRole::Trap)
                .then(|| read_trap_fields(addr))
                .flatten(),
            boom: (role == TrapRole::Boom)
                .then(|| read_boom_fields(addr))
                .flatten(),
        });
    }

    let sim_scale = derive_sim_scale(out);
    for item in out.iter_mut() {
        let rect = world_rect(item, sim_scale);
        item.rect = rect;
    }
    sim_scale
}

/// Имена, которыми игра сама подписывает объект.
///
/// Настоящего имени класса среди них нет и взяться ему неоткуда: у нас есть
/// только указатель на таблицу методов, а вытаскивать имя типа означало бы
/// разбирать внутренности CLR — раскладку `MethodTable` и `EEClass`, которая
/// меняется от версии .NET к версии. Это же — обычные строковые поля объекта,
/// и их достаточно, чтобы отличить пилу от блока земли.
#[derive(Clone, Debug, Default)]
pub struct TrapNames {
    /// `<Name>` (0x20) — имя ассета: `Traps_Small3`, `Object_World5`.
    pub name: String,
    /// `<ObjectType>` (0x10) — категория объекта, если это строка.
    pub object_type: String,
    /// `<TextureName>` (0x0C).
    pub texture: String,
    /// `<ZoneName>` (0x1C).
    pub zone: String,
}

fn read_string_field(addr: usize, offset: usize) -> String {
    mem::read_ptr(addr + offset)
        .and_then(mem::read_dotnet_string)
        .unwrap_or_default()
}

pub fn trap_names(addr: usize) -> TrapNames {
    TrapNames {
        name: read_string_field(addr, trap::NAME),
        object_type: read_string_field(addr, trap::OBJECT_TYPE),
        texture: read_string_field(addr, trap::TEXTURE_NAME),
        zone: read_string_field(addr, trap::ZONE_NAME),
    }
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

    /// Ловушка без единого заполненного источника: тесты дополняют нужные.
    fn blank_trap() -> TrapRef {
        TrapRef {
            addr: 0x2000,
            class: 0x2387_6C30,
            rect: None,
            bounding_raw: None,
            source_raw: None,
            position: Some((0.0, 0.0)),
            position_alt: Some((0.0, 0.0)),
            trap: None,
            boom: None,
        }
    }

    /// Строка `Object_World5` из списка ловушек живой игры.
    fn object_world5() -> TrapRef {
        TrapRef {
            bounding_raw: Some(Rect {
                x: 144,
                y: 576,
                w: 48,
                h: 48,
            }),
            // m_Rectangle: кадр в атласе, y всегда 0.
            source_raw: Some(Rect {
                x: 192,
                y: 0,
                w: 48,
                h: 48,
            }),
            position_alt: Some((1.44, 5.76)),
            ..blank_trap()
        }
    }

    #[test]
    fn world_bounding_wins_over_the_texture_frame() {
        assert_eq!(
            world_rect(&object_world5(), 100.0),
            Some(RectF {
                x: 144.0,
                y: 576.0,
                w: 48.0,
                h: 48.0
            })
        );
    }

    /// У подвижной ловушки базовый `m_Bounding` пуст, а собственный —
    /// заполнен; вторым правилом он и должен подхватываться.
    #[test]
    fn traps_own_bounding_is_used_when_the_inherited_one_is_empty() {
        let item = TrapRef {
            bounding_raw: Some(Rect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            }),
            trap: Some(TrapFields {
                bounding: Some(Rect {
                    x: 1104,
                    y: 624,
                    w: 48,
                    h: 48,
                }),
                texture_size: None,
            }),
            source_raw: Some(Rect {
                x: 96,
                y: 0,
                w: 48,
                h: 48,
            }),
            position_alt: Some((11.04, 6.24)),
            ..blank_trap()
        };
        assert_eq!(
            world_rect(&item, 100.0),
            Some(RectF {
                x: 1104.0,
                y: 624.0,
                w: 48.0,
                h: 48.0
            })
        );
    }

    /// Масштаб симуляции обязан выводиться из данных, а не браться на веру:
    /// ошибка в нём промахивается рамками во столько же раз.
    #[test]
    fn sim_scale_comes_from_objects_that_carry_both_positions() {
        let scale = derive_sim_scale(&[object_world5()]);
        assert!((scale - 100.0).abs() < 0.1, "получили {scale}");
    }

    #[test]
    fn sim_scale_falls_back_when_nothing_carries_both() {
        let mut lonely = object_world5();
        lonely.bounding_raw = None;
        assert_eq!(derive_sim_scale(&[lonely]), SIM_TO_DISPLAY_DEFAULT);
    }

    /// Строка `Traps_SemiMedium2`: `m_Bounding` пуст, `PositionX` нулевая,
    /// и единственная зацепка — позиция в единицах симуляции.
    ///
    /// Регрессия: раньше эти единицы принимались за пиксели, и все такие
    /// ловушки собирались рамками в левом верхнем углу экрана.
    #[test]
    fn simulation_units_are_converted_to_pixels() {
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
        let item = TrapRef {
            bounding_raw: Some(empty),
            source_raw: Some(source),
            position_alt: Some((8.16, 4.8)),
            ..blank_trap()
        };
        let rect = world_rect(&item, 100.0).expect("рамка должна собраться из единиц симуляции");
        // Точного равенства тут не бывает: 4.8 в двоичном виде не
        // представима, и после умножения выходит 480.00003.
        assert!((rect.x - 816.0).abs() < 0.01, "получили {}", rect.x);
        assert!((rect.y - 480.0).abs() < 0.01, "получили {}", rect.y);
        assert_eq!((rect.w, rect.h), (48.0, 96.0));
    }

    #[test]
    fn zero_simulation_position_defers_to_the_pixel_one() {
        let source = Rect {
            x: 0,
            y: 0,
            w: 48,
            h: 48,
        };
        let item = TrapRef {
            source_raw: Some(source),
            position: Some((240.0, 96.0)),
            ..blank_trap()
        };
        assert_eq!(
            world_rect(&item, 100.0).map(|rect| (rect.x, rect.y)),
            Some((240.0, 96.0))
        );
    }

    #[test]
    fn without_any_position_there_is_no_frame() {
        let source = Rect {
            x: 96,
            y: 0,
            w: 48,
            h: 48,
        };
        let item = TrapRef {
            source_raw: Some(source),
            ..blank_trap()
        };
        assert_eq!(world_rect(&item, 100.0), None);
    }

    /// Строка снята с живой игры: `Traps_SemiMedium2` объявляет себя
    /// `BoomTrap` — именно это и избавило от ручной разметки классов.
    #[test]
    fn role_comes_from_the_object_type_string() {
        assert_eq!(TrapRole::from_object_type("BoomTrap"), TrapRole::Boom);
        assert_eq!(TrapRole::from_object_type("Trap"), TrapRole::Trap);
        assert_eq!(TrapRole::from_object_type("QuickGoal"), TrapRole::Base);
        assert_eq!(TrapRole::from_object_type("SPSpawn"), TrapRole::Base);
    }

    /// Пустой или неожиданный тип обязан давать самую осторожную роль:
    /// чтение за 0x90 у короткого класса уходит в соседний объект кучи.
    #[test]
    fn unknown_object_type_stays_on_the_safe_side() {
        assert_eq!(TrapRole::from_object_type(""), TrapRole::Base);
        assert_eq!(TrapRole::from_object_type("что-то новое"), TrapRole::Base);
    }

    #[test]
    fn role_tolerates_namespaces_and_case() {
        assert_eq!(
            TrapRole::from_object_type("Bloody_Trapland.WorldObjects.BoomTrap"),
            TrapRole::Boom
        );
        assert_eq!(TrapRole::from_object_type(" boomtrap "), TrapRole::Boom);
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
