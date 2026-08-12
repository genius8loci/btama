//! Перехват `Player.SetPosition` ради получения указателей на живые объекты.
//!
//! Метод вызывается игрой для каждого объекта каждый кадр, а `this` лежит в
//! `ecx` (соглашение `thiscall`). Детур записывает `ecx` в общий буфер и
//! передаёт управление трамплину MinHook.
//!
//! # Что изменилось против прежней версии
//!
//! * детур оформлен как `#[unsafe(naked)]`-функция с операндами `sym`.
//!   Раньше имена символов были вписаны в ассемблер вручную вместе с
//!   декорацией MSVC (`_OBJECT_COUNT`), из-за чего код молча ломался при
//!   любой смене целевой платформы;
//! * счётчик стал атомарным (`lock xadd`), а не `static mut` с обычным `inc`:
//!   детур работает в потоке игры, а буфер читает поток рендера;
//! * сигнатура обязана совпасть ровно один раз — см. [`crate::scan`];
//! * появились снятие хука и предел числа попыток установки.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use minhook_sys::{MH_CreateHook, MH_DisableHook, MH_EnableHook, MH_OK, MH_RemoveHook};

use crate::log;
use crate::scan::{self, Outcome};

/// Сигнатура пролога `SetPosition`: `add ecx, 0Ch; mov eax, ecx; fld [esp+4]`.
pub const SIGNATURE: &str = "83 C1 0C 8B C1 D9 44 24 04";

/// Вместимость буфера объектов. Метод вызывается по нескольку раз за кадр на
/// объект, поэтому запас нужен заметно больший, чем число игроков.
pub const MAX_OBJECTS: usize = 256;
const MAX_OBJECTS_U32: u32 = MAX_OBJECTS as u32;

/// Сколько раз стерпеть ошибку установки, прежде чем прекратить попытки.
///
/// Считаются только исходы, которые сами не исправятся: ошибка MinHook или
/// неоднозначная сигнатура. Отсутствие совпадений в счёт не идёт — пока
/// игрок не вошёл в уровень, метод не вызывался и JIT его не компилировал,
/// так что это нормальное состояние, а не сбой.
pub const MAX_FAILURES: u32 = 5;

/// Как часто логировать безрезультатные попытки, чтобы не засорять лог.
const QUIET_ATTEMPT_LOG_EVERY: u32 = 10;

/// Буфер указателей на объекты, заполняемый детуром.
///
/// Элементы атомарные, потому что пишет их поток игры, а читает поток
/// рендера. Ассемблер выполняет обычный `mov` — выровненная 4-байтовая
/// запись на x86 атомарна, так что читатель никогда не увидит «половину»
/// указателя.
static OBJECT_BUFFER: [AtomicUsize; MAX_OBJECTS] = [const { AtomicUsize::new(0) }; MAX_OBJECTS];

/// Число записей, сделанных с последнего [`take_objects`].
static OBJECT_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Адрес трамплина MinHook — то, куда детур передаёт управление.
static ORIGINAL_FUNCTION: AtomicUsize = AtomicUsize::new(0);

/// Адрес перехваченной функции; нужен, чтобы снять хук.
static HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);

static INSTALLED: AtomicBool = AtomicBool::new(false);
static SCANNING: AtomicBool = AtomicBool::new(false);
/// Общее число попыток — нужно только для логов.
static ATTEMPTS: AtomicU32 = AtomicU32::new(0);
/// Число неустранимых неудач.
static FAILURES: AtomicU32 = AtomicU32::new(0);

/// Состояние установки хука — показывается в интерфейсе.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Поиск ещё не запускался.
    Idle,
    /// Идёт сканирование памяти.
    Scanning,
    /// Хук установлен, объекты собираются.
    Installed,
    /// Сигнатура не найдена: метод, вероятно, ещё не скомпилирован JIT-ом.
    NotFound,
    /// Сигнатура найдена в нескольких местах — хук не поставлен намеренно.
    Ambiguous(usize),
    /// MinHook вернул ошибку.
    Failed(String),
    /// Исчерпан лимит попыток.
    GaveUp,
}

static STATUS: Mutex<Status> = Mutex::new(Status::Idle);

fn set_status(status: Status) {
    if let Ok(mut slot) = STATUS.lock() {
        *slot = status;
    }
}

/// Текущее состояние. Никогда не блокирует поток рендера: если мьютекс занят
/// потоком сканирования, вернём последнее, что знаем.
pub fn status() -> Status {
    match STATUS.try_lock() {
        Ok(slot) => slot.clone(),
        Err(_) => Status::Scanning,
    }
}

pub fn is_installed() -> bool {
    INSTALLED.load(Ordering::Acquire)
}

/// Осталось ли смысл пробовать снова.
pub fn should_retry() -> bool {
    !is_installed() && FAILURES.load(Ordering::Relaxed) < MAX_FAILURES
}

/// Сбрасывает счётчик неудач и запускает попытку немедленно — по кнопке
/// в интерфейсе, когда автоматические попытки уже прекращены.
pub fn force_retry() {
    FAILURES.store(0, Ordering::Relaxed);
    spawn_install_attempt();
}

// ============================================================================
// ДЕТУР
// ============================================================================

/// Ассемблерный детур.
///
/// Сохраняет регистры и флаги, атомарно занимает слот в буфере, кладёт туда
/// `ecx` (`this`) и прыгает на трамплин. `ebx` не трогается, поэтому и не
/// сохраняется — в прежней версии он сохранялся напрасно.
#[cfg(target_arch = "x86")]
#[unsafe(naked)]
unsafe extern "C" fn detour() {
    core::arch::naked_asm!(
        "push eax",
        "push edx",
        "pushfd",
        // eax = OBJECT_COUNT.fetch_add(1)
        "mov eax, 1",
        "lock xadd dword ptr [{count}], eax",
        // Слот за пределами буфера — просто ничего не пишем.
        "cmp eax, {max}",
        "jae 2f",
        "mov edx, offset {buffer}",
        "mov dword ptr [edx + eax * 4], ecx",
        "2:",
        "popfd",
        "pop edx",
        "pop eax",
        "jmp dword ptr [{original}]",
        count = sym OBJECT_COUNT,
        buffer = sym OBJECT_BUFFER,
        original = sym ORIGINAL_FUNCTION,
        max = const MAX_OBJECTS_U32,
    )
}

// ============================================================================
// УСТАНОВКА И СНЯТИЕ
// ============================================================================

/// Сбрасывает флаг сканирования даже при панике внутри поиска. Прежняя версия
/// теряла флаг навсегда и больше не пыталась поставить хук.
struct ScanGuard;

impl Drop for ScanGuard {
    fn drop(&mut self) {
        SCANNING.store(false, Ordering::Release);
    }
}

/// Запускает попытку установки в фоновом потоке, если её ещё не идёт.
///
/// Сканирование занимает десятки миллисекунд, поэтому в потоке рендера ему
/// не место.
pub fn spawn_install_attempt() {
    if is_installed() {
        return;
    }
    if FAILURES.load(Ordering::Relaxed) >= MAX_FAILURES {
        set_status(Status::GaveUp);
        return;
    }
    // compare_exchange, а не load+store: прежняя проверка допускала вход двух
    // потоков одновременно.
    if SCANNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    // Builder, а не thread::spawn: тот при неудаче паникует, а паниковать
    // в потоке рендера чужого процесса нельзя. При отказе просто снимаем
    // флаг — следующий кадр попробует снова.
    let spawned = std::thread::Builder::new()
        .name("btama-scan".to_string())
        .spawn(|| {
            let _guard = ScanGuard;
            install();
        });

    if let Err(error) = spawned {
        log::error!("не удалось создать поток сканирования: {error}");
        SCANNING.store(false, Ordering::Release);
    }
}

fn install() {
    let attempt = ATTEMPTS.fetch_add(1, Ordering::Relaxed) + 1;
    set_status(Status::Scanning);

    let Some(pattern) = scan::parse(SIGNATURE) else {
        // Сигнатура — константа исходника, сюда можно попасть только по
        // опечатке в ней.
        log::error!("некорректная сигнатура в исходнике: {SIGNATURE:?}");
        fail(Status::Failed("bad signature".into()));
        return;
    };

    match scan::find_unique(&pattern) {
        Outcome::Unique(address) => {
            log::info!("попытка {attempt}: сигнатура найдена по 0x{address:X}");
            create_hook(address);
        }
        Outcome::NotFound => {
            // Ожидаемо до входа в уровень, поэтому не считается неудачей
            // и логируется изредка.
            if attempt % QUIET_ATTEMPT_LOG_EVERY == 1 {
                log::info!("попытка {attempt}: сигнатура не найдена, метод ещё не вызывался");
            }
            set_status(Status::NotFound);
        }
        Outcome::Ambiguous(addresses) => {
            log::warning!(
                "попытка {attempt}: сигнатура неоднозначна ({} совпадений: {}), хук не ставим",
                addresses.len(),
                addresses
                    .iter()
                    .map(|address| format!("0x{address:X}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            fail(Status::Ambiguous(addresses.len()));
        }
    }
}

/// Фиксирует неустранимую неудачу.
fn fail(status: Status) {
    let failures = FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
    if failures >= MAX_FAILURES {
        log::error!("исчерпан лимит неудачных попыток установки хука");
        set_status(Status::GaveUp);
    } else {
        set_status(status);
    }
}

fn create_hook(address: usize) {
    let target = address as *mut c_void;
    let mut trampoline: *mut c_void = std::ptr::null_mut();

    // SAFETY: адрес получен сканированием исполняемой памяти; MinHook сам
    // разбирает пролог и восстанавливает защиту страниц.
    let status =
        unsafe { MH_CreateHook(target, detour as *const () as *mut c_void, &mut trampoline) };
    if status != MH_OK {
        log::error!("MH_CreateHook вернул {status}");
        fail(Status::Failed(format!("MH_CreateHook: {status}")));
        return;
    }
    if trampoline.is_null() {
        log::error!("MH_CreateHook не вернул трамплин");
        // SAFETY: хук был создан вызовом выше, снимаем его же.
        unsafe { MH_RemoveHook(target) };
        fail(Status::Failed("no trampoline".into()));
        return;
    }

    // Трамплин публикуется строго до включения хука: иначе детур мог бы
    // сработать и прыгнуть по нулевому адресу.
    ORIGINAL_FUNCTION.store(trampoline as usize, Ordering::Release);

    // SAFETY: хук создан и трамплин записан.
    let status = unsafe { MH_EnableHook(target) };
    if status != MH_OK {
        log::error!("MH_EnableHook вернул {status}");
        ORIGINAL_FUNCTION.store(0, Ordering::Release);
        // SAFETY: снимаем созданный, но не включённый хук.
        unsafe { MH_RemoveHook(target) };
        fail(Status::Failed(format!("MH_EnableHook: {status}")));
        return;
    }

    HOOK_TARGET.store(address, Ordering::Release);
    INSTALLED.store(true, Ordering::Release);
    set_status(Status::Installed);
    log::info!("хук активен по 0x{address:X}");
}

/// Снимает хук и восстанавливает оригинальный код.
///
/// Обязательно вызывать перед выгрузкой DLL: иначе в игре остаётся `jmp`
/// в освобождённую память, и процесс падает при первом же вызове метода.
pub fn uninstall() {
    if !INSTALLED.swap(false, Ordering::AcqRel) {
        return;
    }
    let target = HOOK_TARGET.swap(0, Ordering::AcqRel);
    if target != 0 {
        // SAFETY: адрес — наш собственный, ранее успешно захученный. MinHook
        // приостанавливает потоки и восстанавливает исходные байты, поэтому
        // после возврата в детур войти уже невозможно.
        unsafe {
            MH_DisableHook(target as *mut c_void);
            MH_RemoveHook(target as *mut c_void);
        }
    }
    // Обнуляем только теперь, когда в детур точно никто не войдёт.
    ORIGINAL_FUNCTION.store(0, Ordering::Release);
    OBJECT_COUNT.store(0, Ordering::Release);
    set_status(Status::Idle);
    log::info!("хук снят");
}

// ============================================================================
// ЧТЕНИЕ БУФЕРА
// ============================================================================

/// Забирает накопленные за кадр указатели в `out` и освобождает буфер.
///
/// `out` переиспользуется вызывающим кодом, чтобы не аллоцировать каждый кадр.
pub fn take_objects(out: &mut Vec<usize>) {
    out.clear();
    if !is_installed() {
        return;
    }

    // Счётчик мог уйти выше вместимости буфера — лишние вызовы просто
    // ничего не записали.
    let count = OBJECT_COUNT.load(Ordering::Acquire).min(MAX_OBJECTS);
    for slot in &OBJECT_BUFFER[..count] {
        let pointer = slot.load(Ordering::Relaxed);
        if pointer != 0 {
            out.push(pointer);
        }
    }
    OBJECT_COUNT.store(0, Ordering::Release);
}

#[cfg(all(test, target_arch = "x86"))]
mod tests {
    use super::*;

    /// Тесты трогают одни и те же статики, а cargo гоняет их параллельно.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Подставляется вместо трамплина MinHook: детур завершается прыжком
    /// сюда, а `ret` возвращает управление в тест.
    unsafe extern "C" fn trampoline_stub() {}

    /// Вызывает детур так же, как это делает игра: `this` в `ecx`.
    fn call_detour(this: usize) {
        // SAFETY: ORIGINAL_FUNCTION указывает на заглушку, которая только
        // возвращает управление. Регистры, которые может испортить вызов,
        // перечислены в списке выходов.
        unsafe {
            core::arch::asm!(
                "call {detour}",
                detour = sym detour,
                inout("ecx") this => _,
                out("eax") _,
                out("edx") _,
            );
        }
    }

    /// Готовит статики к прогону и возвращает охранник блокировки.
    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ORIGINAL_FUNCTION.store(trampoline_stub as *const () as usize, Ordering::Release);
        OBJECT_COUNT.store(0, Ordering::Release);
        for slot in &OBJECT_BUFFER {
            slot.store(0, Ordering::Relaxed);
        }
        INSTALLED.store(true, Ordering::Release);
        guard
    }

    #[test]
    fn detour_records_this_pointer_in_call_order() {
        let _guard = setup();

        for this in [0x1000, 0x2000, 0x3000] {
            call_detour(this);
        }

        let mut objects = Vec::new();
        take_objects(&mut objects);
        assert_eq!(objects, vec![0x1000, 0x2000, 0x3000]);

        INSTALLED.store(false, Ordering::Release);
    }

    #[test]
    fn buffer_is_emptied_between_frames() {
        let _guard = setup();

        call_detour(0xAAAA);
        let mut objects = Vec::new();
        take_objects(&mut objects);
        assert_eq!(objects, vec![0xAAAA]);

        // Второй «кадр» без вызовов детура обязан дать пустой список.
        take_objects(&mut objects);
        assert!(objects.is_empty());

        INSTALLED.store(false, Ordering::Release);
    }

    #[test]
    fn overflow_is_clamped_to_buffer_capacity() {
        let _guard = setup();

        // Заведомо больше вместимости: детур обязан молча отбросить лишнее,
        // а не писать за границу массива.
        for index in 0..MAX_OBJECTS + 32 {
            call_detour(0x10_0000 + index);
        }

        let mut objects = Vec::new();
        take_objects(&mut objects);
        assert_eq!(objects.len(), MAX_OBJECTS);
        assert_eq!(objects[0], 0x10_0000);
        assert_eq!(objects[MAX_OBJECTS - 1], 0x10_0000 + MAX_OBJECTS - 1);

        INSTALLED.store(false, Ordering::Release);
    }
}
