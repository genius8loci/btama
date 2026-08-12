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
use crate::offsets::{METHOD_TABLE, MIN_VALID_ADDRESS, PTR_SIZE, list, player, screen, trap};

/// Верхняя граница разумного размера рамки в игровых единицах.
const MAX_DIMENSION: i32 = 2000;

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
        self.w > 0 && self.h > 0 && self.w <= MAX_DIMENSION && self.h <= MAX_DIMENSION
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

fn is_local(addr: usize) -> bool {
    mem::read::<u8>(addr + player::IS_LOCAL) == Some(1)
}

/// Определяет таблицу методов класса `Player` по самопроверяющемуся признаку.
///
/// Кандидат обязан объявить себя локальным игроком, дать читаемый
/// `GameplayScreen` и найтись в его списке игроков. Совпадение всех трёх
/// условий на случайном мусоре практически исключено.
fn bootstrap(objects: &[usize]) {
    for &addr in objects {
        if !mem::is_readable(addr, player::PROBE_SIZE) || !is_local(addr) {
            continue;
        }
        let Some(screen_addr) = mem::read_ptr(addr + player::GAMEPLAY_SCREEN) else {
            continue;
        };
        if !mem::is_readable(screen_addr, screen::PROBE_SIZE) {
            continue;
        }
        let Some(list_addr) = mem::read_ptr(screen_addr + screen::PLAYERS) else {
            continue;
        };
        if !read_list(list_addr, MAX_PLAYERS).contains(&addr) {
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
        rect: Rect::read(addr + player::BOUNDING_RECT),
        is_local: is_local(addr),
    })
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

    let Some(screen_addr) = mem::read_ptr(local_addr + player::GAMEPLAY_SCREEN) else {
        return world;
    };
    if !mem::is_readable(screen_addr, screen::PROBE_SIZE) {
        return world;
    }
    world.screen = screen_addr;

    // Основной источник — список игроков экрана: он содержит и тех, кто в
    // этом кадре не двигался и потому не попал в буфер хука.
    let mut addresses = mem::read_ptr(screen_addr + screen::PLAYERS)
        .map(|list_addr| read_list(list_addr, MAX_PLAYERS))
        .unwrap_or_default();

    // Запасной вариант — буфер хука. В отличие от прежней версии он отфильтрован
    // по классу, поэтому посторонние объекты в ESP больше не попадают.
    if addresses.is_empty() {
        addresses = objects.to_vec();
    }
    addresses.sort_unstable();
    addresses.dedup();

    world.players = addresses
        .into_iter()
        .filter(|&addr| is_player(addr))
        .filter_map(read_player)
        .collect();

    // Локальный игрок — первым: интерфейс на это опирается.
    world
        .players
        .sort_by_key(|player| if player.is_local { 0 } else { 1 });
    world
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
    pub rect: Option<Rect>,
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
        out.push(TrapRef {
            addr,
            class,
            // Единственная рамка, лежащая по одному адресу и у `Trap`,
            // и у его наследников.
            rect: Rect::read(addr + trap::BOUNDING),
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
