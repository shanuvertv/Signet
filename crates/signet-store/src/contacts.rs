//! The address book.
//!
//! Rows are harvested automatically from every sender, and enriched from whatever their signature
//! offered. One rule governs the whole module: **a person's own edit is never overwritten.** The
//! signature reader works from heuristics over other people's formatting, so it will occasionally
//! be wrong; that is tolerable only while being wrong cannot destroy something somebody typed.
//! `edited_utc` is what marks a row as theirs.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Result, Store, now_utc};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Contact {
    #[serde(default)]
    pub id: i64,
    pub display_name: String,
    pub addr: String,
    #[serde(default)]
    pub job_title: String,
    #[serde(default)]
    pub company: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub mobile: String,
    #[serde(default)]
    pub website: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub times_seen: i64,
    #[serde(default)]
    pub last_seen_utc: i64,
    /// Non-zero once a person has edited this contact by hand.
    #[serde(default)]
    pub edited_utc: i64,
}

/// The details a signature gave up. Mirrors `signet_mime::signature::SignatureDetails`, kept separate
/// so the store does not depend on the MIME crate.
#[derive(Debug, Clone, Default)]
pub struct HarvestedDetails {
    pub job_title: Option<String>,
    pub company: Option<String>,
    pub phone: Option<String>,
    pub mobile: Option<String>,
    pub website: Option<String>,
}

const COLUMNS: &str = "SELECT id, display_name, addr, job_title, company, phone, mobile, website,
                              notes, times_seen, last_seen_utc, edited_utc FROM contacts";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Contact> {
    Ok(Contact {
        id: r.get(0)?,
        display_name: r.get(1)?,
        addr: r.get(2)?,
        job_title: r.get(3)?,
        company: r.get(4)?,
        phone: r.get(5)?,
        mobile: r.get(6)?,
        website: r.get(7)?,
        notes: r.get(8)?,
        times_seen: r.get(9)?,
        last_seen_utc: r.get(10)?,
        edited_utc: r.get(11)?,
    })
}

impl Store {
    /// Every contact, newest correspondents first, optionally filtered.
    ///
    /// The filter matches the company as well as the name and address: "who do we know at Gulf
    /// Lighting" is the question an address book built from a shared sales mailbox gets asked.
    pub fn list_contacts(&self, search: &str, limit: i64) -> Result<Vec<Contact>> {
        self.with_conn(|conn| {
            let term = search.trim();
            let (sql, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = if term.is_empty() {
                (
                    format!("{COLUMNS} ORDER BY times_seen DESC, last_seen_utc DESC LIMIT ?1"),
                    vec![Box::new(limit)],
                )
            } else {
                (
                    format!(
                        "{COLUMNS} WHERE display_name LIKE ?1 OR addr LIKE ?1 OR company LIKE ?1
                         ORDER BY times_seen DESC, last_seen_utc DESC LIMIT ?2"
                    ),
                    vec![Box::new(format!("%{term}%")), Box::new(limit)],
                )
            };

            let mut stmt = conn.prepare(&sql)?;
            let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(refs.as_slice(), row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn get_contact(&self, id: i64) -> Result<Option<Contact>> {
        self.with_conn(|conn| {
            let sql = format!("{COLUMNS} WHERE id = ?1");
            Ok(conn.query_row(&sql, params![id], row).optional()?)
        })
    }

    pub fn contact_count(&self) -> Result<i64> {
        self.with_conn(
            |conn| Ok(conn.query_row("SELECT COUNT(*) FROM contacts", [], |r| r.get(0))?),
        )
    }

    /// Save an edit made by a person, and mark the row as theirs from now on.
    pub fn update_contact(&self, c: &Contact) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE contacts SET display_name = ?2, job_title = ?3, company = ?4, phone = ?5,
                     mobile = ?6, website = ?7, notes = ?8, edited_utc = ?9
                 WHERE id = ?1",
                params![
                    c.id,
                    c.display_name.trim(),
                    c.job_title.trim(),
                    c.company.trim(),
                    c.phone.trim(),
                    c.mobile.trim(),
                    c.website.trim(),
                    c.notes.trim(),
                    now_utc(),
                ],
            )?;
            Ok(())
        })
    }

    pub fn delete_contact(&self, id: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM contacts WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    /// Fill in blanks from a signature, leaving anything already known alone.
    ///
    /// Returns whether anything changed. Two protections: a row a person has edited is never
    /// touched at all, and even on an untouched row only *empty* fields are filled — the first
    /// signature seen is as good a guess as the fortieth, and rewriting a number every time a
    /// slightly different format arrives would make the field flicker.
    pub fn enrich_contact(&self, addr: &str, details: &HarvestedDetails) -> Result<bool> {
        let addr = addr.trim().to_lowercase();
        if addr.is_empty() {
            return Ok(false);
        }

        self.with_conn(|conn| {
            let existing: Option<(i64, i64, String, String, String, String, String)> = conn
                .query_row(
                    "SELECT id, edited_utc, job_title, company, phone, mobile, website
                     FROM contacts WHERE LOWER(addr) = ?1",
                    params![addr],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                        ))
                    },
                )
                .optional()?;

            let Some((id, edited, title, company, phone, mobile, website)) = existing else {
                return Ok(false);
            };
            if edited != 0 {
                return Ok(false);
            }

            let pick = |current: &str, found: &Option<String>| -> Option<String> {
                if !current.trim().is_empty() {
                    return None;
                }
                found
                    .as_ref()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
            };

            let new_title = pick(&title, &details.job_title);
            let new_company = pick(&company, &details.company);
            let new_phone = pick(&phone, &details.phone);
            let new_mobile = pick(&mobile, &details.mobile);
            let new_website = pick(&website, &details.website);

            if new_title.is_none()
                && new_company.is_none()
                && new_phone.is_none()
                && new_mobile.is_none()
                && new_website.is_none()
            {
                return Ok(false);
            }

            conn.execute(
                "UPDATE contacts SET
                     job_title = COALESCE(?2, job_title),
                     company   = COALESCE(?3, company),
                     phone     = COALESCE(?4, phone),
                     mobile    = COALESCE(?5, mobile),
                     website   = COALESCE(?6, website)
                 WHERE id = ?1",
                params![
                    id,
                    new_title,
                    new_company,
                    new_phone,
                    new_mobile,
                    new_website
                ],
            )?;
            Ok(true)
        })
    }
}
