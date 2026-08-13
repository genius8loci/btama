//! Строки интерфейса на двух языках.
//!
//! # Почему пара строк, а не таблица переводов
//!
//! Языка ровно два, и оба известны на этапе компиляции. Таблица с поиском по
//! ключу дала бы возможность промахнуться мимо ключа в рантайме и потребовала
//! бы что-то показывать вместо пропавшего перевода. Пара `&'static str`
//! в одной константе такой возможности не оставляет: если строка объявлена,
//! обе её версии существуют, и проверяет это компилятор.
//!
//! Английский вариант — не подстрочник русского. Термины вроде `Bounding`,
//! `Updateable` или `moveSpeed` в обоих языках остаются как в игре: это имена
//! полей, и переводить их значило бы обрывать связь с исходником.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Ru,
    En,
}

impl Lang {
    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::En,
            _ => Self::Ru,
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::Ru => 0,
            Self::En => 1,
        }
    }
}

/// Выбранный язык. Атомик, а не ячейка: читают его из потока рендера,
/// а `Ordering::Relaxed` здесь достаточно — гонка максимум означает, что
/// один кадр отрисуется на прежнем языке.
static LANG: AtomicU8 = AtomicU8::new(0);

pub fn current() -> Lang {
    Lang::from_code(LANG.load(Ordering::Relaxed))
}

pub fn set(lang: Lang) {
    LANG.store(lang.code(), Ordering::Relaxed);
}

/// Строка на двух языках.
#[derive(Clone, Copy)]
pub struct Text(&'static str, &'static str);

impl Text {
    const fn new(ru: &'static str, en: &'static str) -> Self {
        Self(ru, en)
    }

    pub fn get(self) -> &'static str {
        match current() {
            Lang::Ru => self.0,
            Lang::En => self.1,
        }
    }
}

impl std::fmt::Display for Text {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.get())
    }
}

/// Все строки интерфейса.
pub mod s {
    use super::Text;

    // -- главное окно ------------------------------------------------------
    pub const ESP: Text = Text::new("ESP", "ESP");
    pub const TRAPS: Text = Text::new("Ловушки", "Traps");
    pub const TELEPORT: Text = Text::new("Телепорт", "Teleport");
    pub const TELEPORT_HINT: Text = Text::new(
        "ПКМ мимо окон — телепорт персонажа в точку под курсором",
        "Right-click outside windows to teleport to the cursor",
    );
    pub const DEV: Text = Text::new("DEV", "DEV");
    pub const DEV_HINT: Text = Text::new(
        "Режим разработчика: путь лога, счётчики разбора,\n\
         сырые поля объектов и числа рамок ловушек.\n\
         Нужен при поиске смещений, в игре только мешает.",
        "Developer mode: log path, parser counters, raw object\n\
         fields and trap rectangle numbers.\n\
         Useful when hunting offsets, noise while playing.",
    );
    pub const LANGUAGE_HINT: Text = Text::new(
        "Язык интерфейса. Имена полей игры не переводятся ни в одном:\n\
         Bounding, Updateable и moveSpeed зовутся так в её исходниках.",
        "Interface language. Game field names stay as they are in either:\n\
         Bounding, Updateable and moveSpeed are named so in its sources.",
    );
    pub const NO_PLAYERS: Text = Text::new("Игроки не найдены", "No players found");
    pub const NO_LEVEL: Text = Text::new("Уровень не загружен", "No level loaded");
    pub const REMOTE: Text = Text::new("Сетевой", "Remote");
    pub const RESPAWN: Text = Text::new("Респавн", "Respawn");

    // -- сводка по уровню --------------------------------------------------
    pub const LEVEL_TIME: Text = Text::new("уровень", "level");
    pub const ATTEMPT_TIME: Text = Text::new("попытка", "attempt");
    pub const DEATHS: Text = Text::new("смертей", "deaths");
    pub const KILLS: Text = Text::new("убийств", "kills");
    pub const SECONDS: Text = Text::new("с", "s");

    // -- читы игрока -------------------------------------------------------
    pub const GOD: Text = Text::new("Бессмертие", "God");
    pub const INF_JUMP: Text = Text::new("Беск. прыжок", "Inf jump");
    pub const CROUCH_WALK: Text = Text::new("Ходить в приседе", "Walk while crouched");
    pub const CROUCH_WALK_HINT: Text = Text::new(
        "В HandleInput весь горизонтальный ввод стоит под if (!Crouching),\n\
         поэтому в приседе игра не разгоняет персонажа. Пишем moveSpeed (0xF0)\n\
         сами по стрелкам и A/D — перенос в ApplyGravity про присед не знает\n\
         и всё так же спрашивает MayWalk, так что столкновения на месте.",
        "In HandleInput every horizontal control sits under if (!Crouching),\n\
         so the game never accelerates a crouched character. We write moveSpeed\n\
         (0xF0) ourselves from the arrows and A/D — the actual move in\n\
         ApplyGravity knows nothing about crouching and still calls MayWalk,\n\
         so collisions stay exactly as the game intends.",
    );
    pub const SPEED: Text = Text::new("Скорость", "Speed");
    pub const JUMP: Text = Text::new("Прыжок", "Jump");
    pub const GRAVITY: Text = Text::new("Гравитация", "Gravity");
    pub const RESET: Text = Text::new("Сброс", "Reset");

    // -- окно ловушек ------------------------------------------------------
    pub const TRAP_MANAGER: Text = Text::new("Менеджер Ловушек", "Trap Manager");
    pub const DRAW_TRAP_ESP: Text = Text::new("Рисовать ESP ловушек", "Draw trap ESP");
    pub const LABELS: Text = Text::new("Подписи", "Labels");
    pub const SHOW_RECTS: Text = Text::new("Показать рамки", "Show rectangles");
    pub const SHOW_RECTS_HINT: Text = Text::new(
        "Сырые источники рамки по каждому объекту класса:\n\
         0x50 — m_Bounding, кэш свойства Bounding;\n\
         0x70 — DrawRectangle, кадр в атласе (не мир!);\n\
         0x88 — Position в единицах физического движка;\n\
         масштаб (0x2C) и поворот (0x30).",
        "Raw rectangle sources for every object of the class:\n\
         0x50 — m_Bounding, the cache behind the Bounding property;\n\
         0x70 — DrawRectangle, the atlas frame (not the world!);\n\
         0x88 — Position in physics units;\n\
         scale (0x2C) and rotation (0x30).",
    );
    pub const RAW_FIELDS: Text = Text::new("Сырые поля", "Raw fields");
    pub const RAW_FIELDS_HINT: Text = Text::new(
        "Дамп 0x90..0x140 у первого объекта каждого класса.\n\
         Раскладку неразобранного типа по нему видно глазами:\n\
         значения по умолчанию из исходников узнаются сразу.",
        "Dump of 0x90..0x140 for the first object of each class.\n\
         An unmapped type gives itself away here by eye: default\n\
         values from the sources are recognisable at a glance.",
    );
    pub const TRAPS_TOTAL: Text = Text::new("Всего ловушек", "Traps total");
    pub const SIM_UNIT: Text = Text::new("единица симуляции", "simulation unit");
    pub const NO_RECT: Text = Text::new(
        "Рамка есть только у {with} из {total} — остальные ESP не рисует",
        "Only {with} of {total} have a rectangle — ESP skips the rest",
    );
    pub const UPDATEABLE: Text = Text::new("Обновление", "Update");
    pub const UPDATEABLE_HINT: Text = Text::new(
        "Updateable (0x45) — вызывает ли игра Update этого объекта.\n\
         Самый действенный переключатель: у Spinner в Update и вращение,\n\
         и проверка на убийство. Сняв его, объект замирает целиком.",
        "Updateable (0x45) — whether the game calls this object's Update.\n\
         The most effective switch there is: a Spinner's Update holds both\n\
         its rotation and its kill check. Clear it and the object freezes.",
    );
    pub const CAN_TRIGGER: Text = Text::new("Срабатывание", "Trigger");
    pub const CAN_TRIGGER_HINT: Text = Text::new(
        "m_CanTrigger (0xAC) у BoomTrap.",
        "m_CanTrigger (0xAC) on BoomTrap.",
    );
    pub const BOOM_SPEED_HINT: Text = Text::new(
        "Speed (0xA4) у BoomTrap — пишется во все объекты класса.",
        "Speed (0xA4) on BoomTrap — written to every object of the class.",
    );
    pub const GROUP_ALIGNED: Text = Text::new(
        "Пишется во все объекты класса",
        "Written to every object of the class",
    );
    pub const GROUP_MIXED: Text = Text::new(
        "Значения в группе разошлись — клик выровняет все",
        "Values in the group diverged — a click aligns them all",
    );
    pub const CLASS_TOOLTIP: Text = Text::new(
        "ObjectType (0x10): {kind}\nName (0x20): {name}\nTextureName (0x0C): {texture}\n\
         ZoneName (0x1C): {zone}\nТаблица методов: 0x{class:X}\n\nЧитаем поля: {fields}\n\
         Объектов на уровне: {count}",
        "ObjectType (0x10): {kind}\nName (0x20): {name}\nTextureName (0x0C): {texture}\n\
         ZoneName (0x1C): {zone}\nMethod table: 0x{class:X}\n\nFields read: {fields}\n\
         Objects on the level: {count}",
    );

    // -- диагностика -------------------------------------------------------
    pub const LOG_AT: Text = Text::new("Лог", "Log");
    pub const LOG_FAILED: Text = Text::new(
        "Файл лога не открылся, пишем только в отладчик",
        "Log file could not be opened, writing to the debugger only",
    );
    pub const LOG_NO_REASON: Text = Text::new("причина не записана", "reason not recorded");
    pub const OBJECTS: Text = Text::new("объектов", "objects");
    pub const CLASS: Text = Text::new("класс", "class");
    pub const CLASS_UNKNOWN: Text = Text::new("не опознан", "unknown");
    pub const SCREEN: Text = Text::new("экран", "screen");
    pub const CAMERA: Text = Text::new("камера", "camera");
    pub const CAMERA_NONE: Text = Text::new("нет", "none");
    pub const CAMERA_UNREADABLE: Text = Text::new("не читается", "unreadable");
    pub const ONLINE: Text = Text::new("сеть", "online");
    pub const OFFLINE: Text = Text::new("одиночная", "single");
    pub const UNAVAILABLE: Text = Text::new("недоступно", "unavailable");

    // -- состояние хука ----------------------------------------------------
    pub const SCANNING: Text = Text::new("Сканирование памяти...", "Scanning memory...");
    pub const WAITING: Text = Text::new(
        "Ожидание игры: метод ещё не вызывался",
        "Waiting for the game: the method has not been called yet",
    );
    pub const AMBIGUOUS: Text = Text::new("Сигнатура неоднозначна", "Signature is ambiguous");
    pub const AMBIGUOUS_HINT: Text = Text::new(
        "Хук не установлен намеренно, см. лог",
        "The hook is deliberately not installed, see the log",
    );
    pub const HOOK_ERROR: Text = Text::new("Ошибка MinHook", "MinHook error");
    pub const GAVE_UP: Text = Text::new(
        "Сигнатура не найдена, попытки исчерпаны",
        "Signature not found, out of attempts",
    );
    pub const SEARCH_AGAIN: Text = Text::new("Искать снова", "Search again");
}
