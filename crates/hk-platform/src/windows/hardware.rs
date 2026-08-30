//! The make and model of this computer, like "Lenovo ThinkPad X1 Carbon Gen 12".
//!
//! Windows keeps what the manufacturer put in the firmware in the registry, and
//! manufacturers don't agree on which field holds what. Lenovo's product name is
//! an order code (`21KCCTO1WW`) with the real name in the version field, while
//! Dell and HP put it in the product name. Computers people build themselves often
//! have placeholders like "To Be Filled By O.E.M.", which leaves the motherboard.

use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::core::{HSTRING, PCWSTR, w};

fn firmware_value(name: &str) -> Option<String> {
    let name = HSTRING::from(name);
    let mut buffer = [0u16; 256];
    let mut size = size_of_val(&buffer) as u32;
    // SAFETY: the buffer and its size in bytes are passed together.
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"HARDWARE\DESCRIPTION\System\BIOS"),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if result.is_err() {
        return None;
    }
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..end]))
}

pub(super) fn model() -> Option<String> {
    describe(&Firmware {
        manufacturer: firmware_value("SystemManufacturer"),
        product: firmware_value("SystemProductName"),
        version: firmware_value("SystemVersion"),
        family: firmware_value("SystemFamily"),
        board_manufacturer: firmware_value("BaseBoardManufacturer"),
        board_product: firmware_value("BaseBoardProduct"),
    })
}

#[derive(Default)]
struct Firmware {
    manufacturer: Option<String>,
    product: Option<String>,
    version: Option<String>,
    family: Option<String>,
    board_manufacturer: Option<String>,
    board_product: Option<String>,
}

fn describe(firmware: &Firmware) -> Option<String> {
    let manufacturer = firmware.manufacturer.as_deref().and_then(meaningful);
    let product = firmware.product.as_deref().and_then(meaningful);

    if manufacturer.is_some_and(|name| name.eq_ignore_ascii_case("lenovo")) {
        // The product name is an order code here, and the others say what it is.
        let name = [&firmware.version, &firmware.family]
            .into_iter()
            .filter_map(|value| value.as_deref().and_then(meaningful))
            .find(|value| value.contains(' '))
            .or(product);
        return name.map(|name| with_maker("Lenovo", name));
    }
    if let (Some(manufacturer), Some(product)) = (manufacturer, product) {
        return Some(with_maker(short_maker(manufacturer), product));
    }

    // Built from parts: the motherboard is the most specific thing there is.
    let board_manufacturer = firmware
        .board_manufacturer
        .as_deref()
        .and_then(meaningful)?;
    let board_product = firmware.board_product.as_deref().and_then(meaningful)?;
    Some(with_maker(short_maker(board_manufacturer), board_product))
}

/// Text from the firmware, unless it's one of the placeholders manufacturers
/// leave in there.
fn meaningful(value: &str) -> Option<&str> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    let placeholder = value.is_empty()
        || lower.contains("to be filled")
        || lower.contains("o.e.m.")
        || lower.contains("default string")
        || lower.starts_with("system ")
        || lower == "system"
        || lower == "not applicable"
        || lower == "not specified"
        || lower == "none"
        || lower == "invalid"
        || lower == "unknown"
        || lower == "n/a"
        || lower == "x.x"
        || lower == "0";
    (!placeholder).then_some(value)
}

/// The name people know a manufacturer by.
fn short_maker(manufacturer: &str) -> &str {
    let lower = manufacturer.to_ascii_lowercase();
    const KNOWN: [(&str, &str); 12] = [
        ("dell", "Dell"),
        ("hewlett", "HP"),
        ("asus", "ASUS"),
        ("micro-star", "MSI"),
        ("gigabyte", "Gigabyte"),
        ("microsoft", "Microsoft"),
        ("acer", "Acer"),
        ("samsung", "Samsung"),
        ("toshiba", "Toshiba"),
        ("lg electronics", "LG"),
        ("huawei", "Huawei"),
        ("apple", "Apple"),
    ];
    KNOWN
        .iter()
        .find(|(fragment, _)| lower.contains(fragment))
        .map_or(manufacturer, |(_, short)| short)
}

/// "Dell" and "Latitude 7420" make "Dell Latitude 7420", but "HP" and "HP EliteBook"
/// don't make "HP HP EliteBook".
fn with_maker(maker: &str, model: &str) -> String {
    let already_there = model
        .get(..maker.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(maker));
    if already_there {
        model.to_string()
    } else {
        format!("{maker} {model}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn some(value: &str) -> Option<String> {
        Some(value.to_string())
    }

    #[test]
    fn lenovo_uses_the_name_rather_than_the_order_code() {
        let thinkpad = Firmware {
            manufacturer: some("LENOVO"),
            product: some("21KCCTO1WW"),
            version: some("ThinkPad X1 Carbon Gen 12"),
            family: some("ThinkPad X1 Carbon Gen 12"),
            ..Default::default()
        };
        assert_eq!(
            describe(&thinkpad).as_deref(),
            Some("Lenovo ThinkPad X1 Carbon Gen 12")
        );

        // Some put "Lenovo" in the name themselves, or only have the family.
        let thinkbook = Firmware {
            manufacturer: some("LENOVO"),
            product: some("20VD"),
            version: some("Lenovo ThinkBook 14 G2 ITL"),
            ..Default::default()
        };
        assert_eq!(
            describe(&thinkbook).as_deref(),
            Some("Lenovo ThinkBook 14 G2 ITL")
        );
        let family_only = Firmware {
            manufacturer: some("LENOVO"),
            product: some("81WB"),
            version: some("None"),
            family: some("IdeaPad 3 15IIL05"),
            ..Default::default()
        };
        assert_eq!(
            describe(&family_only).as_deref(),
            Some("Lenovo IdeaPad 3 15IIL05")
        );
    }

    #[test]
    fn lenovo_with_nothing_better_falls_back_to_the_order_code() {
        let bare = Firmware {
            manufacturer: some("LENOVO"),
            product: some("20XW004QUS"),
            ..Default::default()
        };
        assert_eq!(describe(&bare).as_deref(), Some("Lenovo 20XW004QUS"));
    }

    #[test]
    fn common_brands_read_naturally() {
        let cases = [
            ("Dell Inc.", "Latitude 7420", "Dell Latitude 7420"),
            (
                "HP",
                "HP EliteBook 840 G8 Notebook PC",
                "HP EliteBook 840 G8 Notebook PC",
            ),
            ("Hewlett-Packard", "HP ProBook 450 G3", "HP ProBook 450 G3"),
            (
                "ASUSTeK COMPUTER INC.",
                "ROG Zephyrus G14",
                "ASUS ROG Zephyrus G14",
            ),
            (
                "Micro-Star International Co., Ltd.",
                "GS66 Stealth",
                "MSI GS66 Stealth",
            ),
            (
                "Microsoft Corporation",
                "Surface Laptop 5",
                "Microsoft Surface Laptop 5",
            ),
            ("Acer", "Swift SF314-59", "Acer Swift SF314-59"),
            ("Framework", "Laptop 13", "Framework Laptop 13"),
        ];
        for (manufacturer, product, expected) in cases {
            let firmware = Firmware {
                manufacturer: some(manufacturer),
                product: some(product),
                ..Default::default()
            };
            assert_eq!(describe(&firmware).as_deref(), Some(expected));
        }
    }

    #[test]
    fn computers_built_from_parts_name_the_motherboard() {
        let firmware = Firmware {
            manufacturer: some("To Be Filled By O.E.M."),
            product: some("To Be Filled By O.E.M."),
            board_manufacturer: some("ASUSTeK COMPUTER INC."),
            board_product: some("PRIME B550-PLUS"),
            ..Default::default()
        };
        assert_eq!(describe(&firmware).as_deref(), Some("ASUS PRIME B550-PLUS"));

        let firmware = Firmware {
            manufacturer: some("System manufacturer"),
            product: some("System Product Name"),
            board_manufacturer: some("Gigabyte Technology Co., Ltd."),
            board_product: some("B450 AORUS ELITE"),
            ..Default::default()
        };
        assert_eq!(
            describe(&firmware).as_deref(),
            Some("Gigabyte B450 AORUS ELITE")
        );
    }

    #[test]
    fn says_nothing_rather_than_something_meaningless() {
        assert_eq!(describe(&Firmware::default()), None);
        let firmware = Firmware {
            manufacturer: some("Default string"),
            product: some("Default string"),
            board_manufacturer: some("Not Applicable"),
            board_product: some("  "),
            ..Default::default()
        };
        assert_eq!(describe(&firmware), None);
    }

    /// Reads the real registry, which is harmless, so this always runs.
    #[test]
    fn reading_this_computer_does_not_fail() {
        if let Some(model) = model() {
            assert!(!model.trim().is_empty());
        }
    }
}
