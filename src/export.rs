//! Cue list export for the stage manager: CSV for spreadsheets, HTML to print or save as PDF.

use crate::project::{BANKS, CUES_PER_BANK, Project, bank_letter, cue_key};

/// One line per used cue. `lengths[c]` is the formatted play length, if loaded.
pub fn csv(p: &Project, lengths: &[Option<String>]) -> String {
    let field = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    // BOM so Excel opens UTF-8 (accents) correctly.
    let mut out = String::from("\u{feff}Bank,Cue,Name,File,Length,Volume dB,Fade in s,Fade out s,Link,Play over,Notes\n");
    for (c, cue) in p.cues.iter().enumerate().filter(|(_, c)| !c.is_empty()) {
        let file = std::path::Path::new(&cue.path).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let cells = [
            p.banks[c / CUES_PER_BANK].label(c / CUES_PER_BANK),
            cue_key(c),
            cue.name.clone(),
            file,
            lengths[c].clone().unwrap_or_default(),
            format!("{:.1}", cue.gain_db),
            format!("{:.2}", cue.fade_in),
            format!("{:.2}", cue.fade_out),
            if cue.link { "yes" } else { "" }.into(),
            if cue.layer { "yes" } else { "" }.into(),
            cue.notes.clone(),
        ];
        out += &cells.iter().map(|s| field(s)).collect::<Vec<_>>().join(",");
        out.push('\n');
    }
    out
}

/// Printable page: one table per bank, notes included.
pub fn html(p: &Project, lengths: &[Option<String>]) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\n', "<br>");
    let mut out = format!(
        "<!doctype html><meta charset=utf-8><title>{name}</title><style>\
         body{{font:14px system-ui,sans-serif;margin:24px;color:#111}}h1{{margin:0 0 4px}}\
         h2{{margin:24px 0 6px;border-bottom:2px solid #d81e1e}}table{{border-collapse:collapse;width:100%}}\
         td,th{{border:1px solid #ccc;padding:4px 6px;text-align:left;vertical-align:top}}th{{background:#eee}}\
         .k{{white-space:nowrap;font-weight:600}}</style><h1>{name}</h1><p>Cue list, {date}</p>",
        name = esc(&p.name),
        date = crate::project::today()
    );
    for b in 0..BANKS {
        let cues: Vec<usize> = (b * CUES_PER_BANK..(b + 1) * CUES_PER_BANK).filter(|&c| !p.cues[c].is_empty()).collect();
        if cues.is_empty() {
            continue;
        }
        out += &format!(
            "<h2>{} · {}</h2><table><tr><th>Cue</th><th>Name</th><th>Length</th><th>Link</th><th>Notes</th></tr>",
            bank_letter(b),
            esc(&p.banks[b].label(b))
        );
        for c in cues {
            let cue = &p.cues[c];
            out += &format!(
                "<tr><td class=k>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                cue_key(c),
                esc(&cue.name),
                lengths[c].clone().unwrap_or_default(),
                if cue.link { "→" } else { "" },
                esc(&cue.notes)
            );
        }
        out += "</table>";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_and_html_escapes() {
        let mut p = Project::default();
        p.cues[0].path = "/a/b.wav".into();
        p.cues[0].name = "Say \"hi\", then <go>".into();
        p.cues[0].notes = "line 1\nline 2".into();
        let lengths = vec![Some("00:03:000".into()); 50];
        let c = csv(&p, &lengths);
        assert!(c.contains("\"Say \"\"hi\"\", then <go>\""), "{c}");
        assert!(c.contains("\"line 1\nline 2\""));
        assert_eq!(c.lines().filter(|l| l.contains("b.wav")).count(), 1);
        let h = html(&p, &lengths);
        assert!(h.contains("Say \"hi\", then &lt;go&gt;") && h.contains("line 1<br>line 2"));
    }
}
