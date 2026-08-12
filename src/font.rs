//! Шрифт интерфейса с поддержкой кириллицы.
//!
//! # Зачем
//!
//! Встроенный в ImGui шрифт ProggyClean содержит только латиницу. Пока весь
//! интерфейс был англоязычным, это не мешало, но русский текст выводится им
//! как последовательность `?`. Поэтому шрифт заменяется системным, а нужные
//! диапазоны глифов задаются явно.
//!
//! # Почему это работает
//!
//! hudhook вызывает [`crate::overlay::CheatOverlay::initialize`] в
//! `Pipeline::new` **до** `engine.setup_fonts`, который и строит текстуру
//! атласа. Значит, добавленный здесь шрифт попадает в атлас штатным образом,
//! без пересборки текстуры.

use imgui::{Context, FontConfig, FontGlyphRanges, FontSource};

use crate::log;

/// Кегль в пикселях. ProggyClean по умолчанию 13; берём чуть крупнее, потому
/// что векторный шрифт в мелком кегле читается хуже растрового.
const FONT_SIZE: f32 = 15.0;

/// Кандидаты в порядке предпочтения.
///
/// Consolas первым намеренно: список ловушек выравнивает колонки пробелами
/// (`{:<20}`), а это осмысленно только в моноширинном шрифте. Остальные —
/// запасные варианты на случай урезанной установки Windows.
const CANDIDATES: &[&str] = &["consola.ttf", "tahoma.ttf", "segoeui.ttf", "arial.ttf"];

/// Диапазоны глифов.
///
/// `FontGlyphRanges::cyrillic()` не подошёл: он покрывает латиницу и
/// кириллицу, но не общую пунктуацию, из-за чего длинное тире в подсказках
/// осталось бы пустым квадратом. Диапазоны обязаны идти без пересечений и
/// заканчиваться нулём — иначе `from_slice` паникует.
static GLYPH_RANGES: &[u32] = &[
    0x0020, 0x00FF, // латиница, знаки препинания, «ёлочки»
    0x0400, 0x052F, // кириллица и дополнение к ней
    0x2010, 0x2027, // тире, типографские кавычки, многоточие
    0x2116, 0x2116, // знак номера
    0,
];

/// Каталог системных шрифтов Windows.
fn fonts_dir() -> std::path::PathBuf {
    std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
        .join("Fonts")
}

/// Устанавливает шрифт с кириллицей.
///
/// Если ни один кандидат не прочитался, остаётся встроенный ProggyClean и
/// русский текст снова будет нечитаем — поэтому неудача попадает в лог.
pub fn install(ctx: &mut Context) {
    let dir = fonts_dir();

    for name in CANDIDATES {
        let path = dir.join(name);
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(error) => {
                log::info!("шрифт {} недоступен: {error}", path.display());
                continue;
            }
        };

        // Атлас копирует данные себе (`FontDataOwnedByAtlas`), поэтому
        // временный буфер можно спокойно уронить после вызова.
        ctx.fonts().add_font(&[FontSource::TtfData {
            data: &data,
            size_pixels: FONT_SIZE,
            config: Some(FontConfig {
                size_pixels: FONT_SIZE,
                glyph_ranges: FontGlyphRanges::from_slice(GLYPH_RANGES),
                ..FontConfig::default()
            }),
        }]);

        log::info!("шрифт интерфейса: {}", path.display());
        return;
    }

    log::warning!(
        "в {} не нашлось ни одного шрифта с кириллицей; \
         русский текст в оверлее будет нечитаем",
        dir.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `from_slice` паникует на неверно составленном списке, поэтому важно,
    /// чтобы константа проходила его проверки: нечётная длина, нулевой
    /// терминатор, отсутствие пересечений.
    #[test]
    fn glyph_ranges_are_well_formed() {
        assert_eq!(GLYPH_RANGES.len() % 2, 1, "длина обязана быть нечётной");
        assert_eq!(GLYPH_RANGES.last(), Some(&0), "нужен нулевой терминатор");

        let pairs: Vec<(u32, u32)> = GLYPH_RANGES[..GLYPH_RANGES.len() - 1]
            .chunks_exact(2)
            .map(|pair| (pair[0], pair[1]))
            .collect();

        for &(start, end) in &pairs {
            assert!(
                start != 0 && start <= end,
                "некорректный диапазон {start:#x}..={end:#x}"
            );
        }
        for window in pairs.windows(2) {
            assert!(
                window[0].1 < window[1].0,
                "диапазоны {:?} и {:?} пересекаются",
                window[0],
                window[1]
            );
        }
    }

    /// Символы, реально встречающиеся в строках интерфейса, обязаны попадать
    /// в объявленные диапазоны — иначе на экране будет пустой квадрат.
    #[test]
    fn ui_characters_are_covered() {
        let covered = |ch: char| {
            GLYPH_RANGES[..GLYPH_RANGES.len() - 1]
                .chunks_exact(2)
                .any(|pair| (pair[0]..=pair[1]).contains(&(ch as u32)))
        };

        for ch in "Сканирование памяти... Ожидание игры: метод ещё не вызывался".chars()
        {
            assert!(covered(ch), "символ {ch:?} вне диапазонов");
        }
        for ch in ['—', '«', '»', '…', '№', 'ё', 'Ё', 'A', '0', '#'] {
            assert!(covered(ch), "символ {ch:?} вне диапазонов");
        }
    }
}
