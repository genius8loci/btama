//! Читерский модуль для Bloody Trapland (XNA / .NET Framework, 32 бита).
//!
//! DLL инжектируется в процесс игры и предоставляет:
//!
//! * ESP игроков и ловушек;
//! * правку параметров локального персонажа (скорость, прыжок, гравитация,
//!   неуязвимость, бесконечный прыжок);
//! * просмотр и правку флагов ловушек;
//! * интерфейс ImGui поверх DirectX 9.
//!
//! # Устройство
//!
//! | Модуль | Ответственность |
//! |---|---|
//! | [`scan`] | поиск сигнатуры в исполняемой памяти |
//! | [`hook`] | детур `SetPosition` и сбор указателей на объекты |
//! | [`mem`] | проверяемый доступ к памяти игры |
//! | [`offsets`] | смещения полей, снятые с дампов в `re/` |
//! | [`game`] | разбор игровых объектов |
//! | [`overlay`] | интерфейс и применение читов |
//! | [`log`] | диагностика в `OutputDebugStringW` и файл |
//!
//! # Управление
//!
//! * `INSERT` — закрепить/убрать меню;
//! * `TAB` — показать меню на время удержания;
//! * `END` — снять хуки и выгрузить DLL.

// DllMain обязан называться именно так.
#![allow(non_snake_case)]

// Детур написан на 32-битном ассемблере, а все смещения предполагают
// 4-байтовый указатель. Собранная под x86_64 DLL всё равно не инжектится
// в 32-битный процесс, поэтому лучше упасть на этапе компиляции.
#[cfg(not(target_arch = "x86"))]
compile_error!(
    "BTamaCheat собирается только под i686-pc-windows-msvc. \
     Цель задана в .cargo/config.toml — не переопределяйте её."
);

mod game;
mod hook;
mod log;
mod mem;
mod offsets;
mod overlay;
mod scan;

use std::ffi::c_void;
use std::sync::Once;

use hudhook::Hudhook;
use hudhook::hooks::dx9::ImguiDx9Hooks;
use minhook_sys::{MH_ERROR_ALREADY_INITIALIZED, MH_Initialize, MH_OK};
use windows::Win32::Foundation::{BOOL, HINSTANCE, HMODULE};
use windows::Win32::System::SystemServices::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};

use overlay::CheatOverlay;

/// Смещения в [`offsets`] описывают 32-битную раскладку объектов.
const _: () = assert!(
    size_of::<usize>() == offsets::PTR_SIZE,
    "смещения рассчитаны на 32-битный процесс"
);

/// Направляет паники в лог: `stderr` в инжектированной DLL никто не читает.
fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let location = info
                .location()
                .map(|at| format!("{}:{}", at.file(), at.line()))
                .unwrap_or_else(|| "неизвестно где".to_string());
            let message = info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
                .unwrap_or("<без сообщения>");
            log::error!("паника в {location}: {message}");
        }));
    });
}

/// Инициализация, вынесенная из `DllMain` в отдельный поток.
///
/// В самом `DllMain` держится блокировка загрузчика, под которой нельзя
/// вызывать почти ничего; поэтому вся работа выносится наружу, а
/// `DllMain` только создаёт поток и сразу возвращает управление.
fn initialize() {
    // Ставится здесь, а не в DllMain: `set_hook` выделяет память, а под
    // блокировкой загрузчика этого лучше не делать.
    install_panic_hook();
    log::info!("инициализация, лог: {}", log::log_path().display());

    // MinHook общий на процесс: его же использует hudhook, который
    // терпимо относится к повторной инициализации.
    // SAFETY: вызов без аргументов, безопасен для повторного вызова.
    let status = unsafe { MH_Initialize() };
    if status != MH_OK && status != MH_ERROR_ALREADY_INITIALIZED {
        log::error!("MH_Initialize вернул {status}, дальше идти незачем");
        return;
    }

    // Первая попытка поставить хук делается сразу; если метод ещё не
    // скомпилирован JIT-ом, оверлей повторит попытку сам.
    hook::spawn_install_attempt();
}

fn attach(module: HMODULE) {
    let instance = HINSTANCE(module.0);

    std::thread::spawn(move || {
        initialize();

        let overlay = Hudhook::builder()
            .with::<ImguiDx9Hooks>(CheatOverlay::new())
            .with_hmodule(instance)
            .build()
            .apply();

        // Прежняя версия здесь делала .unwrap(): паника в потоке чужого
        // процесса убивала игру вместо тихого отказа.
        match overlay {
            Ok(()) => log::info!("оверлей DX9 активен"),
            Err(status) => log::error!("не удалось применить hudhook: {status:?}"),
        }
    });
}

/// `reserved` от загрузчика: ненулевое значение означает, что выгружается
/// не библиотека, а весь процесс. В этом случае зависимости могут быть уже
/// выгружены, а все прочие потоки — остановлены, поэтому убирать за собой
/// и незачем, и опасно.
fn detach(reserved: *mut c_void) {
    if !reserved.is_null() {
        return;
    }
    log::info!("выгрузка: снимаем хуки");
    hook::uninstall();
    game::forget_cached_pointers();
    mem::invalidate();
}

/// Точка входа DLL.
///
/// # Safety
///
/// Вызывается только загрузчиком Windows с корректными аргументами.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(module: HMODULE, reason: u32, reserved: *mut c_void) -> BOOL {
    match reason {
        DLL_PROCESS_ATTACH => attach(module),
        DLL_PROCESS_DETACH => detach(reserved),
        _ => {}
    }
    BOOL(1)
}
