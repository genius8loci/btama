//! Проверяемый доступ к памяти игры.
//!
//! # Зачем это нужно
//!
//! Игра написана на .NET, а его сборщик мусора уплотняет кучу и **двигает**
//! объекты. Указатель, полученный кадр назад, может уже указывать на мусор
//! или на освобождённый регион. Прежняя версия разыменовывала такие адреса
//! напрямую, проверяя лишь диапазон `0x10000..0x7FFFFFFF`, — отсюда брались
//! случайные вылеты игры.
//!
//! Поймать нарушение доступа из Rust на MSVC нечем: это SEH-исключение, а не
//! паника, и `catch_unwind` его не видит. Значит, проверять адрес нужно
//! *до* обращения.
//!
//! # Как это сделано
//!
//! Перед каждым доступом адрес сверяется с картой регионов процесса
//! (`VirtualQuery`). Ответы кэшируются: managed-объекты живут в считанных
//! сегментах GC-кучи, поэтому кэш почти всегда попадает и проверка стоит
//! несколько инструкций вместо системного вызова. Кэш сбрасывается каждые
//! [`CACHE_TTL_FRAMES`] кадров, чтобы освобождённый регион не считался
//! доступным вечно.
//!
//! Защита от чтения мусора *внутри* живого региона — уже не наша задача:
//! ею занимается сверка таблицы методов в [`crate::game`].

use std::cell::RefCell;
use std::ffi::c_void;

use windows::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_PROTECTION_FLAGS, PAGE_READONLY, PAGE_READWRITE,
    PAGE_WRITECOPY, VirtualQuery,
};

use crate::offsets::{MIN_VALID_ADDRESS, string};

/// Через столько кадров карта регионов считается устаревшей и сбрасывается.
/// Полсекунды при 60 FPS: достаточно редко, чтобы кэш окупался, и достаточно
/// часто, чтобы освобождённый сегмент кучи не оставался «доступным».
const CACHE_TTL_FRAMES: u32 = 30;

/// Больше этого числа регионов кэш не растит — при переполнении просто
/// начинает заново. На практике managed-куча укладывается в единицы записей.
const MAX_CACHED_REGIONS: usize = 64;

#[derive(Clone, Copy)]
struct Region {
    start: usize,
    end: usize,
    readable: bool,
    writable: bool,
}

#[derive(Default)]
struct Cache {
    regions: Vec<Region>,
    frames: u32,
}

thread_local! {
    /// Кэш живёт в TLS, а не в глобальном мьютексе: обращения к памяти игры
    /// идут только из потока рендера, так что блокировки были бы чистыми
    /// накладными расходами. Побочный эффект — сигнатуры функций остаются
    /// свободными от `&mut self`, что заметно упрощает код интерфейса.
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

/// Отмечает начало кадра и периодически сбрасывает карту регионов.
pub fn tick() {
    CACHE.with_borrow_mut(|cache| {
        cache.frames += 1;
        if cache.frames >= CACHE_TTL_FRAMES {
            cache.frames = 0;
            cache.regions.clear();
        }
    });
}

/// Немедленно сбрасывает карту регионов.
pub fn invalidate() {
    CACHE.with_borrow_mut(|cache| {
        cache.frames = 0;
        cache.regions.clear();
    });
}

/// Флаги защиты страниц — это значения в младшем байте, а не набор битов,
/// поэтому сравниваем с маской, а не проверяем побитовым «и».
fn access_of(protect: PAGE_PROTECTION_FLAGS) -> (bool, bool) {
    // Страница-сторож валидна, но первое же обращение к ней возбуждает
    // исключение, поэтому для нас она недоступна.
    if protect.0 & PAGE_GUARD.0 != 0 {
        return (false, false);
    }
    let base = protect.0 & 0xFF;
    let readable = base == PAGE_READONLY.0
        || base == PAGE_READWRITE.0
        || base == PAGE_WRITECOPY.0
        || base == PAGE_EXECUTE_READ.0
        || base == PAGE_EXECUTE_READWRITE.0
        || base == PAGE_EXECUTE_WRITECOPY.0;
    let writable = base == PAGE_READWRITE.0
        || base == PAGE_WRITECOPY.0
        || base == PAGE_EXECUTE_READWRITE.0
        || base == PAGE_EXECUTE_WRITECOPY.0;
    (readable, writable)
}

/// Общая часть [`is_readable`] и [`is_writable`].
fn check(addr: usize, len: usize, want_write: bool) -> bool {
    if addr < MIN_VALID_ADDRESS || len == 0 {
        return false;
    }
    let Some(end) = addr.checked_add(len) else {
        return false;
    };

    CACHE.with_borrow_mut(|cache| {
        if let Some(region) = cache
            .regions
            .iter()
            .find(|region| region.start <= addr && end <= region.end)
        {
            return if want_write {
                region.writable
            } else {
                region.readable
            };
        }

        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: VirtualQuery принимает произвольный адрес — невалидный он
        // сообщает возвратом 0, а не падением. Буфер наш и нужного размера.
        let queried = unsafe {
            VirtualQuery(
                Some(addr as *const c_void),
                &mut mbi,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if queried == 0 {
            return false;
        }

        let start = mbi.BaseAddress as usize;
        let Some(region_end) = start.checked_add(mbi.RegionSize) else {
            return false;
        };
        let (readable, writable) = if mbi.State == MEM_COMMIT {
            access_of(mbi.Protect)
        } else {
            (false, false)
        };

        if cache.regions.len() >= MAX_CACHED_REGIONS {
            cache.regions.clear();
        }
        cache.regions.push(Region {
            start,
            end: region_end,
            readable,
            writable,
        });

        // Запрошенный диапазон может выходить за границу региона — тогда
        // соседний регион надо проверять отдельно, а мы просто отказываем.
        end <= region_end && if want_write { writable } else { readable }
    })
}

/// Доступны ли `len` байт по адресу `addr` для чтения.
pub fn is_readable(addr: usize, len: usize) -> bool {
    check(addr, len, false)
}

/// Доступны ли `len` байт по адресу `addr` для записи.
pub fn is_writable(addr: usize, len: usize) -> bool {
    check(addr, len, true)
}

/// Читает значение из памяти игры, если адрес действительно доступен.
pub fn read<T: Copy>(addr: usize) -> Option<T> {
    if !is_readable(addr, size_of::<T>()) {
        return None;
    }
    // SAFETY: доступность диапазона только что проверена, `T: Copy` — значит,
    // произвольная битовая комбинация не нарушит инвариантов владения.
    // `read_unaligned`, потому что managed-поля выровнены лишь на 4 байта.
    Some(unsafe { (addr as *const T).read_unaligned() })
}

/// Записывает значение в память игры. Возвращает `false`, если адрес
/// недоступен для записи — вызывающий код так узнаёт, что объект исчез.
pub fn write<T: Copy>(addr: usize, value: T) -> bool {
    if !is_writable(addr, size_of::<T>()) {
        return false;
    }
    // SAFETY: доступность на запись только что проверена.
    unsafe { (addr as *mut T).write_unaligned(value) };
    true
}

/// Читает указатель игры (в 32-битном процессе — 4 байта).
pub fn read_ptr(addr: usize) -> Option<usize> {
    read::<u32>(addr).map(|value| value as usize)
}

/// Читает `System.String` .NET Framework.
///
/// Возвращает `None` и на недоступный адрес, и на неправдоподобную длину:
/// и то и другое означает, что по адресу лежит не строка.
pub fn read_dotnet_string(addr: usize) -> Option<String> {
    let len = read::<i32>(addr + string::LENGTH)?;
    if len <= 0 || len > string::MAX_CHARS {
        return None;
    }
    let len = len as usize;
    let chars = addr + string::FIRST_CHAR;
    if !is_readable(chars, len * size_of::<u16>()) {
        return None;
    }
    // SAFETY: диапазон проверен; `chars` кратен 4, а значит выровнен под u16.
    let slice = unsafe { std::slice::from_raw_parts(chars as *const u16, len) };
    Some(String::from_utf16_lossy(slice))
}
