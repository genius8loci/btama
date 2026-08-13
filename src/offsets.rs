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
    /// `m_Position` (2 × f32). Запись сюда телепортирует персонажа.
    pub const POSITION: usize = 0x0C;
    /// `m_Color` (u32) — упакованный XNA-цвет в порядке RGBA.
    pub const COLOR: usize = 0x38;
    /// `m_BoundingRect` (4 × i32: X, Y, W, H).
    pub const BOUNDING_RECT: usize = 0x3C;
    /// `m_Name` — указатель на `System.String`.
    pub const NAME: usize = 0x54;
    /// `RemotePlayer` (u8) — управляется ли игрок по сети.
    ///
    /// Нужен вдобавок к [`IS_LOCAL`]: в одиночной игре сеть не участвует и
    /// `isLocal` остаётся нулём даже у нашего собственного персонажа, тогда
    /// как `RemotePlayer` равен нулю в обоих режимах.
    pub const REMOTE_PLAYER: usize = 0x6D;
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

/// `Bloody_Trapland.Screens.GameplayScreen` — базовый класс экрана.
///
/// В одиночной игре экран имеет именно этот тип, в сетевой — производный
/// `OnlineGameplayScreen`. Все смещения ниже принадлежат базовому классу и
/// потому верны в обоих режимах.
///
/// Ровно на этом раньше всё и ломалось: код искал игроков по `m_Players`
/// (0x100), а это поле есть **только** у производного онлайнового класса.
/// В одиночной игре по 0x100 лежит что угодно, список не читался, класс
/// `Player` не опознавался — и разом отказывали и список игроков, и ESP.
///
/// Общего `PROBE_SIZE` для экрана намеренно нет: требовать читаемости всей
/// структуры целиком нельзя, длина у двух классов разная.
pub mod screen {
    /// `TrapList` — `List<Trap>`.
    pub const TRAP_LIST: usize = 0x34;
    /// `PlayerList` — `List<Player>`, локальные игроки.
    pub const PLAYER_LIST: usize = 0x3C;
    /// `RemotePlayerList` — `List<Player>`, сетевые игроки.
    pub const REMOTE_PLAYER_LIST: usize = 0x40;
    /// `MergedPlayerList` — `List<Player>`, объединение двух предыдущих.
    pub const MERGED_PLAYER_LIST: usize = 0x44;
    /// `camera` — указатель на `TwoPlayGame.Cameras.Camera`.
    /// Раскладку см. в [`super::camera`].
    pub const CAMERA: usize = 0x4C;
    /// `m_Online` (u8) — сетевая ли сессия.
    pub const IS_ONLINE: usize = 0xD1;
}

/// `Bloody_Trapland.WorldObjects.Trap` и его наследники.
///
/// Всё до 0x90 принадлежит общему предку `TwoPlayGame.GameWorld.WorldObject`
/// и потому одинаково у `Trap`, `BoomTrap`, `QuickGoal` и прочих элементов
/// списка ловушек.
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
    /// `<PositionX>k__BackingField` и `<PositionY>k__BackingField` (2 × f32) —
    /// мировая позиция объекта, левый верхний угол.
    pub const POSITION_X: usize = 0x24;
    /// `Used` (u8).
    pub const USED: usize = 0x44;
    /// `Updateable` (u8).
    pub const UPDATEABLE: usize = 0x45;
    /// `GoreStick` (u8).
    pub const GORE_STICK: usize = 0x46;
    /// `m_Bounding` (4 × i32) — прямоугольник коллизии **в мировых
    /// координатах**. Единственный источник рамки, который годится для ESP
    /// как есть, но заполнен не у всех объектов: у собственно ловушек он
    /// нередко остаётся нулевым, и тогда рамку приходится собирать из
    /// [`POSITION_X`] и размера [`SOURCE_RECT`].
    pub const BOUNDING: usize = 0x50;
    /// `m_Rectangle` (4 × i32) — **кадр в атласе текстур**, а не положение
    /// в мире.
    ///
    /// Здесь и была причина того, что ESP ловушек рисовался кучей в углу
    /// экрана: прежняя версия считала это поле основным источником рамки.
    /// Снятые с игры числа не оставляют сомнений — у всех объектов `y = 0`,
    /// а `x` кратен 48 (`96,0 48x48`, `48,0 48x96`, `192,0 48x48`), то есть
    /// это раскладка спрайтов в текстуре. Полезен из него только размер:
    /// он совпадает с размером объекта в мире.
    pub const SOURCE_RECT: usize = 0x70;
    /// `<Position>k__BackingField` (2 × f32) — положение в **единицах
    /// физического движка**, а не в пикселях.
    ///
    /// Единственный источник, заполненный у всех объектов: [`POSITION_X`]
    /// в снятой сборке везде нулевая, а [`BOUNDING`] есть только у элементов
    /// уровня. Числа с живой игры не оставляют сомнений в том, что это за
    /// единицы: у объекта с `m_Bounding = 1488,576` здесь лежит `14.88,5.76`.
    /// Множитель выводится из этих же пар в `game::derive_sim_scale`.
    pub const POSITION: usize = 0x88;

    /// `Speed` (f32) — **только** у `BoomTrap`-подобных классов.
    pub const BOOM_SPEED: usize = 0xA4;
    /// `m_CanTrigger` (u8) — **только** у `BoomTrap`-подобных классов.
    pub const BOOM_CAN_TRIGGER: usize = 0xAC;

    /// Достаточно для полей, общих для всех ловушек.
    pub const PROBE_SIZE: usize = POSITION + 2 * 4;
    /// Достаточно для полей `BoomTrap`.
    pub const BOOM_PROBE_SIZE: usize = BOOM_CAN_TRIGGER + 1;
}

/// `TwoPlayGame.Cameras.Camera` — то, чем игра переводит мир в экран.
///
/// Наследование: `Camera` → `BaseCamera` → `BaseObject` → `CoreObject`.
///
/// С 0x30 начинается сплошная лента матриц 4×4 по 64 байта: `Viewprojection`
/// (0x30), `m_Projection` (0x70), `m_SimProjection` (0xB0), `m_Transform`
/// (0xF0), `m_InvertTransform` (0x130), `m_InvertScalelessTransform` (0x170).
/// Из них нам нужна ровно одна, остальные перечислены, чтобы соседние
/// смещения не выглядели произвольными. `m_Rotation` (0x24) здесь тоже нет:
/// поворот уже сведён в матрицу.
pub mod camera {
    /// `m_Zoom` (f32).
    pub const ZOOM: usize = 0x20;
    /// `ViewportWidth` (i32).
    pub const VIEWPORT_WIDTH: usize = 0x28;
    /// `ViewportHeight` (i32).
    pub const VIEWPORT_HEIGHT: usize = 0x2C;
    /// `m_Transform` (`Matrix`, 16 × f32, построчно) — ровно то, что игра
    /// отдаёт в `SpriteBatch.Begin`. Берём её целиком, а не собираем
    /// преобразование из зума и позиции: масштаб, поворот и центрирование
    /// вьюпорта уже сведены здесь в шесть чисел.
    pub const TRANSFORM: usize = 0xF0;
    /// `m_CameraPosition` (2 × f32).
    pub const CAMERA_POSITION: usize = 0x1D8;
    /// `RealPosition` (2 × f32).
    pub const REAL_POSITION: usize = 0x1E0;

    /// Сколько байт камеры должно читаться, прежде чем трогать её поля.
    pub const PROBE_SIZE: usize = REAL_POSITION + 2 * 4;
}

/// Указатель на таблицу методов (в дампах Cheat Engine — «Vtable») лежит по
/// нулевому смещению у любого managed-объекта и одинаков для всех экземпляров
/// одного класса. Мы используем его двояко: как проверку, что по адресу
/// действительно живёт объект ожидаемого типа, и как идентификатор класса
/// ловушки.
pub const METHOD_TABLE: usize = 0x00;
