//! Поиск сигнатуры в исполняемой памяти процесса.
//!
//! Код игры — управляемый: JIT компилирует метод при первом вызове в
//! анонимный регион, адрес которого меняется от запуска к запуску. Поэтому
//! функцию приходится искать по байтам её машинного кода, а не по смещению
//! от модуля.
//!
//! Отличия от прежней версии:
//!
//! * сначала просматривается только приватная память (`MEM_PRIVATE`) — там и
//!   только там живут кодовые кучи CLR. Раньше сканировались вдобавок все
//!   системные модули, что и медленно, и чревато ложным совпадением;
//! * собираются **все** совпадения. Если сигнатура неоднозначна, хук не
//!   ставится вовсе: перехватить наугад не ту функцию хуже, чем не
//!   перехватить ничего;
//! * пропускаются страницы-сторожа и некоммитированные регионы.

use std::ffi::c_void;

use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_PROTECTION_FLAGS, VirtualQuery,
};

/// Сколько совпадений имеет смысл собрать: всё, что больше одного, уже
/// означает непригодную сигнатуру.
const MATCH_LIMIT: usize = 8;

/// Результат поиска.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Совпадений нет — как правило, метод ещё не был вызван и не
    /// скомпилирован JIT-ом.
    NotFound,
    /// Ровно одно совпадение: только в этом случае можно ставить хук.
    Unique(usize),
    /// Несколько совпадений — сигнатуру нужно удлинять.
    Ambiguous(Vec<usize>),
}

/// Разбирает сигнатуру вида `"83 C1 0C ? 8B C1"`, где `?` — любой байт.
pub fn parse(signature: &str) -> Option<Vec<Option<u8>>> {
    let pattern: Option<Vec<Option<u8>>> = signature
        .split_whitespace()
        .map(|token| match token {
            "?" | "??" => Some(None),
            hex => u8::from_str_radix(hex, 16).ok().map(Some),
        })
        .collect();
    pattern.filter(|pattern: &Vec<Option<u8>>| !pattern.is_empty())
}

/// Ищет все вхождения сигнатуры в срезе и складывает абсолютные адреса
/// (`base + смещение`) в `out`, пока их не наберётся `limit`.
///
/// Вынесено из работы с Windows API отдельно, чтобы поведение поиска можно
/// было проверить обычными тестами.
fn find_in_slice(
    haystack: &[u8],
    pattern: &[Option<u8>],
    base: usize,
    out: &mut Vec<usize>,
    limit: usize,
) {
    if pattern.is_empty() || haystack.len() < pattern.len() {
        return;
    }
    let last = haystack.len() - pattern.len();
    let first = pattern[0];

    let mut i = 0;
    while i <= last {
        // Быстрый пропуск до следующего кандидата по первому байту.
        if let Some(byte) = first {
            match haystack[i..=last].iter().position(|&x| x == byte) {
                Some(offset) => i += offset,
                None => return,
            }
        }

        let matches = pattern
            .iter()
            .zip(&haystack[i..])
            .all(|(expected, &actual)| expected.is_none_or(|byte| byte == actual));
        if matches {
            out.push(base + i);
            if out.len() >= limit {
                return;
            }
        }
        i += 1;
    }
}

/// Пригоден ли регион для сканирования исполняемого кода.
fn is_scannable(mbi: &MEMORY_BASIC_INFORMATION, private_only: bool) -> bool {
    if mbi.State != MEM_COMMIT {
        return false;
    }
    if private_only && mbi.Type != MEM_PRIVATE {
        return false;
    }
    is_executable_readable(mbi.Protect)
}

fn is_executable_readable(protect: PAGE_PROTECTION_FLAGS) -> bool {
    if protect.0 & PAGE_GUARD.0 != 0 {
        return false;
    }
    // PAGE_EXECUTE (0x10) намеренно исключён: он исполняемый, но не читаемый.
    let base = protect.0 & 0xFF;
    base == PAGE_EXECUTE_READ.0
        || base == PAGE_EXECUTE_READWRITE.0
        || base == PAGE_EXECUTE_WRITECOPY.0
}

/// Обходит адресное пространство и собирает совпадения.
fn collect(pattern: &[Option<u8>], private_only: bool) -> Vec<usize> {
    let mut found = Vec::new();
    let mut address: usize = 0;

    loop {
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: VirtualQuery безопасен для любого адреса и сообщает о
        // невалидном возвратом 0.
        let queried = unsafe {
            VirtualQuery(
                Some(address as *const c_void),
                &mut mbi,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if queried == 0 {
            break;
        }

        let start = mbi.BaseAddress as usize;
        let size = mbi.RegionSize;

        if size > 0 && is_scannable(&mbi, private_only) {
            // SAFETY: регион закоммичен и доступен на чтение (страницы-сторожа
            // отфильтрованы). Гонка с освобождением региона другим потоком
            // здесь принципиально неустранима, но кодовые кучи CLR за время
            // жизни процесса не освобождаются.
            let region = unsafe { std::slice::from_raw_parts(start as *const u8, size) };
            find_in_slice(region, pattern, start, &mut found, MATCH_LIMIT);
            if found.len() >= MATCH_LIMIT {
                break;
            }
        }

        // Переход к следующему региону; на 32 битах адрес рано или поздно
        // выходит за границу пользовательского пространства и VirtualQuery
        // возвращает 0. checked_add страхует от зацикливания при переполнении.
        match start.checked_add(size) {
            Some(next) if next > address => address = next,
            _ => break,
        }
    }

    found
}

/// Ищет сигнатуру: сначала в приватной памяти, затем, если там пусто, во всей
/// исполняемой — на случай, если код всё-таки лежит в образе модуля.
pub fn find_unique(pattern: &[Option<u8>]) -> Outcome {
    let mut found = collect(pattern, true);
    if found.is_empty() {
        found = collect(pattern, false);
    }

    match found.len() {
        0 => Outcome::NotFound,
        1 => Outcome::Unique(found[0]),
        _ => Outcome::Ambiguous(found),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_all(haystack: &[u8], pattern: &[Option<u8>]) -> Vec<usize> {
        let mut out = Vec::new();
        find_in_slice(haystack, pattern, 0, &mut out, MATCH_LIMIT);
        out
    }

    #[test]
    fn parses_signature_with_wildcards() {
        let pattern = parse("83 C1 0C ? 8B").expect("валидная сигнатура");
        assert_eq!(
            pattern,
            vec![Some(0x83), Some(0xC1), Some(0x0C), None, Some(0x8B)]
        );
    }

    #[test]
    fn rejects_malformed_signature() {
        assert!(parse("83 ZZ").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn finds_single_match_at_correct_offset() {
        let haystack = [0x00, 0x11, 0x83, 0xC1, 0x0C, 0x22];
        let pattern = parse("83 C1 0C").unwrap();
        assert_eq!(find_all(&haystack, &pattern), vec![2]);
    }

    #[test]
    fn wildcard_matches_any_byte() {
        let haystack = [0x83, 0xFF, 0x0C];
        let pattern = parse("83 ? 0C").unwrap();
        assert_eq!(find_all(&haystack, &pattern), vec![0]);
    }

    #[test]
    fn reports_every_match() {
        let haystack = [0xAA, 0xBB, 0x00, 0xAA, 0xBB];
        let pattern = parse("AA BB").unwrap();
        assert_eq!(find_all(&haystack, &pattern), vec![0, 3]);
    }

    #[test]
    fn match_at_the_very_end_is_found() {
        let haystack = [0x00, 0xAA, 0xBB];
        let pattern = parse("AA BB").unwrap();
        assert_eq!(find_all(&haystack, &pattern), vec![1]);
    }

    #[test]
    fn pattern_longer_than_haystack_finds_nothing() {
        let haystack = [0xAA];
        let pattern = parse("AA BB CC").unwrap();
        assert!(find_all(&haystack, &pattern).is_empty());
    }

    #[test]
    fn leading_wildcard_does_not_break_the_scan() {
        let haystack = [0x01, 0xBB, 0x02, 0xBB];
        let pattern = parse("? BB").unwrap();
        assert_eq!(find_all(&haystack, &pattern), vec![0, 2]);
    }

    #[test]
    fn absolute_addresses_are_offset_by_base() {
        let haystack = [0x00, 0xAA];
        let pattern = parse("AA").unwrap();
        let mut out = Vec::new();
        find_in_slice(&haystack, &pattern, 0x1000, &mut out, MATCH_LIMIT);
        assert_eq!(out, vec![0x1001]);
    }

    #[test]
    fn stops_at_the_match_limit() {
        let haystack = [0xAA; 64];
        let pattern = parse("AA").unwrap();
        assert_eq!(find_all(&haystack, &pattern).len(), MATCH_LIMIT);
    }
}
