//! Writing the address book out as vCard or CSV.
//!
//! Two formats because they answer different questions. **vCard** is what Outlook, a phone and
//! Google Contacts all import without being told anything; it is the right default for "put these
//! people in my address book". **CSV** is what goes into a spreadsheet, which is what actually
//! happens to a supplier list.
//!
//! The CSV writer escapes formulas. Contact names arrive from strangers' signatures, and Excel
//! treats a leading `=`, `+`, `-` or `@` in a cell as the start of a formula — so a display name of
//! `=cmd|'/c calc'!A0` is a working attack on whoever opens the export. Quoting alone does not stop
//! it; the cell has to stop being a formula.

use signet_store::Contact;

/// Characters Excel and LibreOffice treat as the start of a formula.
const FORMULA_STARTERS: [char; 5] = ['=', '+', '-', '@', '\t'];

/// One CSV cell: quoted, doubled quotes, and defused if it looks like a formula.
fn csv_cell(value: &str) -> String {
    let mut v = value.replace(['\r', '\n'], " ");

    // A leading apostrophe is the conventional way to tell a spreadsheet "this is text". It is
    // added rather than the character stripped, so nothing is silently lost from the data.
    if v.chars()
        .next()
        .is_some_and(|c| FORMULA_STARTERS.contains(&c))
    {
        v.insert(0, '\'');
    }

    format!("\"{}\"", v.replace('"', "\"\""))
}

/// The whole address book as CSV, with a header row.
///
/// Column names match what Outlook's own import wizard expects, so the file can be handed straight
/// to it rather than mapped field by field.
pub fn to_csv(contacts: &[Contact]) -> String {
    let mut out = String::from(
        "\"Name\",\"Email Address\",\"Job Title\",\"Company\",\"Business Phone\",\
         \"Mobile Phone\",\"Web Page\",\"Notes\",\"Messages\"\n",
    );

    for c in contacts {
        let row = [
            csv_cell(&c.display_name),
            csv_cell(&c.addr),
            csv_cell(&c.job_title),
            csv_cell(&c.company),
            csv_cell(&c.phone),
            csv_cell(&c.mobile),
            csv_cell(&c.website),
            csv_cell(&c.notes),
            csv_cell(&c.times_seen.to_string()),
        ];
        out.push_str(&row.join(","));
        out.push('\n');
    }
    out
}

/// Escape a value for a vCard property. Commas, semicolons and backslashes are structural.
fn vcard_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace('\r', "")
        .replace('\n', "\\n")
}

/// Split a display name into family and given parts for `N`.
///
/// Crude on purpose. Names do not decompose reliably, and guessing elaborately is worse than
/// guessing simply: `FN` carries the name as written, which is what every importer displays.
fn name_parts(display: &str) -> (String, String) {
    let mut words = display.split_whitespace();
    let given = words.next().unwrap_or("").to_string();
    let family = words.next_back().unwrap_or("").to_string();
    if family == given {
        (String::new(), given)
    } else {
        (family, given)
    }
}

/// The whole address book as vCard 3.0.
///
/// 3.0 rather than 4.0: it is what Outlook and Android import without complaint, and nothing here
/// needs anything 4.0 added.
pub fn to_vcard(contacts: &[Contact]) -> String {
    let mut out = String::new();

    for c in contacts {
        let display = if c.display_name.trim().is_empty() {
            c.addr.clone()
        } else {
            c.display_name.clone()
        };
        let (family, given) = name_parts(&display);

        out.push_str("BEGIN:VCARD\r\nVERSION:3.0\r\n");
        out.push_str(&format!("FN:{}\r\n", vcard_value(&display)));
        out.push_str(&format!(
            "N:{};{};;;\r\n",
            vcard_value(&family),
            vcard_value(&given)
        ));
        if !c.addr.trim().is_empty() {
            out.push_str(&format!(
                "EMAIL;TYPE=INTERNET,WORK:{}\r\n",
                vcard_value(&c.addr)
            ));
        }
        if !c.company.trim().is_empty() {
            out.push_str(&format!("ORG:{}\r\n", vcard_value(&c.company)));
        }
        if !c.job_title.trim().is_empty() {
            out.push_str(&format!("TITLE:{}\r\n", vcard_value(&c.job_title)));
        }
        if !c.phone.trim().is_empty() {
            out.push_str(&format!(
                "TEL;TYPE=WORK,VOICE:{}\r\n",
                vcard_value(&c.phone)
            ));
        }
        if !c.mobile.trim().is_empty() {
            out.push_str(&format!("TEL;TYPE=CELL:{}\r\n", vcard_value(&c.mobile)));
        }
        if !c.website.trim().is_empty() {
            out.push_str(&format!("URL:{}\r\n", vcard_value(&c.website)));
        }
        if !c.notes.trim().is_empty() {
            out.push_str(&format!("NOTE:{}\r\n", vcard_value(&c.notes)));
        }
        out.push_str("END:VCARD\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(name: &str, addr: &str) -> Contact {
        Contact {
            display_name: name.into(),
            addr: addr.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_formula_in_a_name_cannot_execute_in_excel() {
        // The attack this exists to stop. Names come from strangers' signatures, and whoever opens
        // the export is the target rather than us.
        let c = contact("=cmd|'/c calc'!A0", "a@b.test");
        let csv = to_csv(&[c]);
        assert!(
            csv.contains("\"'=cmd"),
            "a formula was written as a live cell: {csv}"
        );
    }

    #[test]
    fn every_formula_starter_is_defused() {
        for prefix in ['=', '+', '-', '@'] {
            let csv = to_csv(&[contact(&format!("{prefix}SUM(A1)"), "a@b.test")]);
            assert!(
                csv.contains(&format!("\"'{prefix}SUM")),
                "{prefix} was left live"
            );
        }
    }

    #[test]
    fn ordinary_names_are_left_exactly_as_they_are() {
        // Defusing must not mangle the overwhelming majority of real names.
        let csv = to_csv(&[contact("Amir Rahman", "amir@gulf.test")]);
        assert!(csv.contains("\"Amir Rahman\""), "{csv}");
        assert!(
            !csv.contains('\''),
            "an apostrophe was added to a safe name"
        );
    }

    #[test]
    fn quotes_and_commas_survive_the_round_trip() {
        let csv = to_csv(&[contact("Rahman, \"Amir\"", "a@b.test")]);
        assert!(csv.contains("\"Rahman, \"\"Amir\"\"\""), "{csv}");
    }

    #[test]
    fn newlines_in_a_field_cannot_break_the_row_apart() {
        let mut c = contact("Amir", "a@b.test");
        c.notes = "line one\nline two".into();
        let csv = to_csv(&[c]);
        // One header row plus one data row plus the trailing newline.
        assert_eq!(csv.lines().count(), 2, "a field split the row: {csv}");
    }

    #[test]
    fn a_vcard_carries_the_details_worth_carrying() {
        let mut c = contact("Amir Rahman", "amir@gulf.test");
        c.company = "Gulf Lighting LLC".into();
        c.job_title = "Sales Manager".into();
        c.mobile = "+971 50 111 2222".into();

        let v = to_vcard(&[c]);
        assert!(v.contains("FN:Amir Rahman"));
        assert!(v.contains("N:Rahman;Amir;;;"));
        assert!(v.contains("EMAIL;TYPE=INTERNET,WORK:amir@gulf.test"));
        assert!(v.contains("ORG:Gulf Lighting LLC"));
        assert!(v.contains("TITLE:Sales Manager"));
        assert!(v.contains("TEL;TYPE=CELL:+971 50 111 2222"));
        assert!(v.starts_with("BEGIN:VCARD\r\nVERSION:3.0\r\n"));
        assert!(v.trim_end().ends_with("END:VCARD"));
    }

    #[test]
    fn vcard_structural_characters_are_escaped() {
        // A comma in a company name would otherwise split the property into two values.
        let mut c = contact("Amir", "a@b.test");
        c.company = "Gulf Lighting, LLC; Dubai".into();
        let v = to_vcard(&[c]);
        assert!(v.contains("ORG:Gulf Lighting\\, LLC\\; Dubai"), "{v}");
    }

    #[test]
    fn a_contact_with_no_name_falls_back_to_the_address() {
        // Harvested contacts often have an address and nothing else; an empty FN imports as a
        // blank row that nobody can identify.
        let v = to_vcard(&[contact("", "unknown@supplier.test")]);
        assert!(v.contains("FN:unknown@supplier.test"), "{v}");
    }

    #[test]
    fn a_single_word_name_does_not_become_its_own_surname() {
        let v = to_vcard(&[contact("Amir", "a@b.test")]);
        assert!(v.contains("N:;Amir;;;"), "{v}");
    }

    #[test]
    fn empty_fields_are_left_out_rather_than_written_blank() {
        // An importer shown `TEL:` with nothing after it creates an empty phone entry.
        let v = to_vcard(&[contact("Amir", "a@b.test")]);
        assert!(!v.contains("TEL"), "{v}");
        assert!(!v.contains("ORG:"), "{v}");
        assert!(!v.contains("NOTE:"), "{v}");
    }
}
