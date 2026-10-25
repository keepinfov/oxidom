//! Local page export; deliberately excludes raw logs, configuration and paths.

use oxidom_core::client::DaemonSource;
use oxidom_core::diagnostics::Page;
use oxidom_core::redact::{Redactor, ReportContext, report};
use oxidom_core::versions::Versions;

pub fn render(page: &Page, source: Option<DaemonSource>) -> anyhow::Result<String> {
    let versions = Versions::here(
        env!("CARGO_PKG_VERSION"),
        Some("not collected"),
        Some("not collected"),
        source,
    );
    render_with(page, source, versions, Redactor::here())
}

fn render_with(
    page: &Page,
    source: Option<DaemonSource>,
    mut versions: Versions,
    mut redactor: Redactor,
) -> anyhow::Result<String> {
    // Environment-derived headings are text too; the shared formatter only
    // redacts its log lines, not the Versions/ReportContext headings.
    for value in [&mut versions.distribution, &mut versions.desktop]
        .into_iter()
        .flatten()
    {
        *value = redactor.line(value);
    }
    let context = ReportContext {
        transport: Some("not collected (persistent diagnostics only)".into()),
        user_agent: "not collected".into(),
    };
    let mut lines = vec![format!(
        "Persistent diagnostics: {}; page version {}; gap={}; rejected={}; incomplete_tail={}; more={}",
        if source.is_some() {
            "selected daemon"
        } else {
            "explicit local file (no daemon contacted)"
        },
        page.version,
        page.gap,
        page.rejected,
        page.incomplete_tail,
        page.more
    )];
    lines.extend(
        page.records
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?,
    );
    let mut output = report(&versions, &context, &lines, &mut redactor);
    output.push_str(
        "\nExport limits\n\
         This is one bounded page, not a complete or atomic history. Raw memory logs, \
         server identities, settings and journal paths were not collected. A gap means \
         continuity could not be proved; the lost count is unknown and the page restarts \
         at the oldest retained data, which can repeat previously read records. Rejected \
         lines were consumed without exporting their text. An incomplete current tail \
         remains unread. Review the entire report before sharing; nothing is uploaded.\n",
    );
    // Only a reader-generated position is included, never the supplied cursor.
    // Keep it outside shape redaction so it remains usable for continuation.
    if let Some(cursor) = &page.next_cursor {
        output.push_str(&format!("Next cursor: {cursor}\n"));
    } else {
        output.push_str("Next cursor: none\n");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_exposes_page_limits_without_copying_unknown_journal_secrets() {
        let page: Page = serde_json::from_str(
            r#"{"version":1,"records":[{"version":1,"unix_ms":1,"pid":42,"event":"phase","phase":"starting","secret":"invented-private-secret"}],"next_cursor":null,"gap":true,"rejected":2,"incomplete_tail":true,"more":false}"#,
        )
        .unwrap();
        let versions = Versions {
            app: "test".into(),
            daemon: Some("not collected".into()),
            core: Some("not collected".into()),
            source: None,
            install: oxidom_core::versions::Install::Unknown,
            distribution: Some("https://invented-provider.example/private-token".into()),
            desktop: Some("invented-host".into()),
        };
        let output = render_with(
            &page,
            None,
            versions,
            Redactor::with(Some("invented-host"), None),
        )
        .unwrap();
        assert!(output.starts_with("oxidom problem report\n"));
        assert!(output.contains("explicit local file (no daemon contacted)"));
        assert!(output.contains("gap=true; rejected=2; incomplete_tail=true; more=false"));
        assert!(output.contains("\"phase\":\"starting\""));
        assert!(output.contains("not a complete or atomic history"));
        assert!(output.contains("nothing is uploaded"));
        assert!(output.ends_with("Next cursor: none\n"));
        for secret in [
            "invented-private-secret",
            "invented-provider.example",
            "private-token",
            "invented-host",
        ] {
            assert!(!output.contains(secret), "{secret}");
        }
    }
}
