//! Смещения полей игровых объектов.
//!
//! Значения взяты из дампов структур Cheat Engine в каталоге `re/` и названы
//! так же, как поля в игре. Раньше они были рассыпаны по коду голыми числами,
//! из-за чего два места независимо трактовали одно и то же смещение
//! по-разному — см. [`trap`].
//!
//! Игра 32-битная, поэтому указатель и элемент managed-массива занимают
//! ровно 4 байта; это зафиксировано в [`PTR_SIZE`] и проверяется в `lib.rs`.

/// Размер указателя в адресном пространстве игры.
pub const PTR_SIZE: usize = 4;

/// Адрес ниже этой границы заведомо не является объектом: первые 64 КБ
/// адресного пространства Windows резервирует и никогда не отдаёт.
pub const MIN_VALID_ADDRESS: usize = 0x1_0000;

/// `System.String` в .NET Framework (32 бита).
pub mod string {
    /// `m_stringLength` (i32) — длина в символах UTF-16.
    pub const LENGTH: usize = 0x04;
    /// `m_firstChar` — начало массива UTF-16.
    pub const FIRST_CHAR: usize = 0x08;
    /// Верхняя граница разумной длины имени. Всё, что длиннее, означает,
    /// что мы читаем не строку, а случайный мусор.
    pub const MAX_CHARS: i32 = 256;
}

/// `System.Collections.Generic.List<T>` (32 бита).
pub mod list {
    /// `_items` — указатель на managed-массив.
    pub const ITEMS: usize = 0x04;
    /// `_size` (i32) — фактическое число элементов.
    pub const SIZE: usize = 0x0C;
    /// Смещение первого элемента внутри managed-массива.
    pub const ARRAY_DATA: usize = 0x08;
}

/// `Bloody_Trapland.GameObjects.Player`.
pub mod player {
    /// `m_UniqueID` (i32) — стабильный на сессию идентификатор. Именно он,
    /// а не адрес объекта, служит ключом пользовательских настроек: адрес
    /// меняется при каждой компактящей сборке мусора.
    pub const UNIQUE_ID: usize = 0x04;
    /// `m_Position` (2 × f32).
    pub const POSITION: usize = 0x0C;
    /// `m_Color` (u32) — упакованный XNA-цвет в порядке RGBA.
    pub const COLOR: usize = 0x38;
    /// `m_BoundingRect` (4 × i32: X, Y, W, H).
    pub const BOUNDING_RECT: usize = 0x3C;
    /// `m_Name` — указатель на `System.String`.
    pub const NAME: usize = 0x54;
    /// `GameplayScreen` — указатель на экран, через который доступны
    /// списки игроков и ловушек.
    pub const GAMEPLAY_SCREEN: usize = 0xAC;
    /// `DeltaSpeed` (f32) — то, что интерфейс показывает как «Speed».
    /// Обратите внимание: это не `moveSpeed` (0xF0) и не `PlayerSpeed` (0x108).
    pub const DELTA_SPEED: usize = 0xE0;
    /// `InitialJumpHeight` (f32).
    pub const INITIAL_JUMP_HEIGHT: usize = 0xEC;
    /// `GravityAmountDefault` (f32).
    pub const GRAVITY_DEFAULT: usize = 0x10C;
    /// `gravityAmount` (f32).
    pub const GRAVITY: usize = 0x110;
    /// `Alive` (u8) — сброс в 0 приводит к респавну.
    pub const ALIVE: usize = 0x142;
    /// `JumpBeenReleased` (u8).
    pub const JUMP_BEEN_RELEASED: usize = 0x143;
    /// `CanJump` (u8).
    pub const CAN_JUMP: usize = 0x144;
    /// `canDie` (u8).
    pub const CAN_DIE: usize = 0x150;
    /// `isLocal` (u8).
    pub const IS_LOCAL: usize = 0x152;

    /// Сколько байт объекта должно быть доступно для чтения, прежде чем
    /// трогать любое из полей выше.
    pub const PROBE_SIZE: usize = IS_LOCAL + 1;
}

/// `Bloody_Trapland.Screens.OnlineGameplayScreen`.
///
/// # Осторожно
///
/// Дампы в `re/` сняты с **онлайновой** сессии. В одиночной игре класс экрана
/// другой, и эти смещения могут не значить ничего. Поэтому оба списка ниже —
/// не более чем подсказка: результат обязательно проверяется, а при неудаче
/// код обходится буфером хука. Общего `PROBE_SIZE` для экрана намеренно нет —
/// требовать читаемости всех 0x104 байт нельзя, экран может быть короче.
pub mod screen {
    /// `TrapList` — `List<Trap>`.
    pub const TRAP_LIST: usize = 0x34;
    /// `m_Players` — `List<Player>`.
    pub const PLAYERS: usize = 0x100;
}

/// `Bloody_Trapland.WorldObjects.Trap` и его наследники.
///
/// # Почему 0xA4 и 0xAC вынесены отдельно
///
/// У базового `Trap` по 0xA4..0xB0 лежит второй `m_Bounding` (4 × i32),
/// а у наследника `BoomTrap` по тем же адресам — `Speed` (f32, 0xA4) и
/// `m_CanTrigger` (u8, 0xAC). Прежняя версия писала туда `Speed`
/// и `CanTrigger` для *любого* элемента списка ловушек и тем самым
/// затирала прямоугольник коллизии у всех обычных ловушек.
///
/// Поэтому эти два поля доступны только для классов, которые пользователь
/// явно пометил как `BoomTrap` в интерфейсе; классы различаются по
/// указателю на таблицу методов в [`METHOD_TABLE`].
pub mod trap {
    /// `<Name>k__BackingField` — указатель на `System.String`.
    pub const NAME: usize = 0x20;
    /// `Used` (u8).
    pub const USED: usize = 0x44;
    /// `Updateable` (u8).
    pub const UPDATEABLE: usize = 0x45;
    /// `GoreStick` (u8).
    pub const GORE_STICK: usize = 0x46;
    /// `m_Rectangle` (4 × i32) — основной источник рамки для ESP.
    ///
    /// Именно это смещение использовала последняя версия, у которой ESP
    /// ловушек работал. Присутствует и у `Trap`, и у `BoomTrap`.
    pub const RECTANGLE: usize = 0x70;
    /// `m_Bounding` (4 × i32) — запасной источник рамки, тоже общий для
    /// `Trap` и `BoomTrap`.
    pub const BOUNDING: usize = 0x50;

    /// `Speed` (f32) — **только** у `BoomTrap`-подобных классов.
    pub const BOOM_SPEED: usize = 0xA4;
    /// `m_CanTrigger` (u8) — **только** у `BoomTrap`-подобных классов.
    pub const BOOM_CAN_TRIGGER: usize = 0xAC;

    /// Достаточно для полей, общих для всех ловушек.
    pub const PROBE_SIZE: usize = RECTANGLE + 4 * 4;
    /// Достаточно для полей `BoomTrap`.
    pub const BOOM_PROBE_SIZE: usize = BOOM_CAN_TRIGGER + 1;
}

/// Указатель на таблицу методов (в дампах Cheat Engine — «Vtable») лежит по
/// нулевому смещению у любого managed-объекта и одинаков для всех экземпляров
/// одного класса. Мы используем его двояко: как проверку, что по адресу
/// действительно живёт объект ожидаемого типа, и как идентификатор класса
/// ловушки.
pub const METHOD_TABLE: usize = 0x00;
