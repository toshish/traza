//! Agent Blame unit tests.

use serde_json::{json, Value};

use super::*;
use crate::tape::tests::{span, TempStore};

/// A scope over `path` in every project (no validation, so the store's own
/// refusals are reachable).
fn anywhere(path: &str) -> BlameScope {
    BlameScope {
        path: normalize_path(path),
        projects: Vec::new(),
    }
}

fn tool_span(trace: &str, id: &str, start: u64, name: &str, arguments: Value) -> Span {
    span(
        trace,
        id,
        start,
        json!({
            "gen_ai.operation.name": "execute_tool",
            "gen_ai.tool.name": name,
            "gen_ai.tool.call.arguments": arguments,
        }),
    )
}

#[test]
fn lines_and_paths_normalise() {
    assert_eq!(normalize_line("  let  x =\t1;  "), "let x = 1;");
    assert_eq!(normalize_line(" \t "), "");
    assert_eq!(normalize_path("./src//a/./b.rs/"), "src/a/b.rs");
    assert_eq!(normalize_path("C:\\repo\\src\\b.rs"), "C:/repo/src/b.rs");
    assert_eq!(normalize_path("/abs//x"), "/abs/x");
}

#[test]
fn a_path_matches_by_component_suffix_only() {
    assert!(path_matches("/repo/src/lib.rs", "src/lib.rs"));
    assert!(path_matches("/repo/src/lib.rs", "lib.rs"));
    assert!(path_matches("/repo/src/lib.rs", "/repo/src/lib.rs"));
    assert!(
        path_matches("/home/ci/repo/src/lib.rs", "/repo/src/lib.rs"),
        "a leading slash is not an anchor"
    );
    assert!(
        !path_matches("/repo/src/glib.rs", "lib.rs"),
        "not a component"
    );
    assert!(!path_matches("/repo/src/lib.rs", "src/main.rs"));
    // A recorded path that stayed relative matches a longer query.
    assert!(path_matches("src/lib.rs", "/checkout/src/lib.rs"));
    assert!(!path_matches("/elsewhere/lib.rs", "/checkout/src/lib.rs"));
    assert!(!path_matches("", "lib.rs"));
}

#[test]
fn an_edit_introduces_a_line_only_when_it_adds_more_than_it_removes() {
    let edit = tool_span(
        "t",
        "e",
        1,
        "Edit",
        json!({
            "file_path": "/repo/src/cart.rs",
            "old_string": "struct Cart;\nfn keep() {}",
            "new_string": "struct Cart;\nfn keep() {}\nfn total() -> u64 { 0 }",
        }),
    );
    let edits = edits(&edit);
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].file, "/repo/src/cart.rs");
    assert!(edits[0].introduces(&normalize_line("fn total() -> u64 { 0 }")));
    assert!(
        !edits[0].introduces(&normalize_line("fn keep() {}")),
        "a context line the edit kept is not authorship"
    );
    // Re-indenting is not authorship either.
    let reindent = Edit {
        added: vec!["        x();".into()],
        removed: vec!["    x();".into()],
        ..Edit::default()
    };
    assert!(!reindent.introduces("x();"));
}

#[test]
fn every_edit_shape_is_read() {
    let multi = tool_span(
        "t",
        "m",
        1,
        "MultiEdit",
        json!({"file_path": "/r/a.rs", "edits": [
            {"old_string": "a", "new_string": "a\nb"},
            {"old_string": "c", "new_string": "d"},
        ]}),
    );
    let read = edits(&multi);
    assert_eq!(read[0].added, ["a", "b", "d"]);
    assert_eq!(read[0].removed, ["a", "c"]);

    let write = tool_span(
        "t",
        "w",
        1,
        "Write",
        json!({"file_path": "/r/b.rs", "content": "x\ny\n"}),
    );
    assert_eq!(edits(&write)[0].added, ["x", "y"]);
    // `content` on a tool that is not a writer is not an edit.
    let other = tool_span(
        "t",
        "o",
        1,
        "Slack",
        json!({"path": "/r/c.rs", "content": "x"}),
    );
    assert!(edits(&other).is_empty());

    let notebook = tool_span(
        "t",
        "n",
        1,
        "NotebookEdit",
        json!({"notebook_path": "/r/n.ipynb", "new_source": "print(1)"}),
    );
    assert_eq!(edits(&notebook)[0].added, ["print(1)"]);

    let editor = tool_span(
        "t",
        "s",
        1,
        "str_replace_based_edit_tool",
        json!({"command": "str_replace", "path": "/r/d.py", "old_str": "a = 1", "new_str": "a = 2"}),
    );
    assert_eq!(edits(&editor)[0].added, ["a = 2"]);

    // Arguments recorded as a JSON string parse the same.
    let stringly = tool_span(
        "t",
        "j",
        1,
        "Write",
        json!("{\"file_path\":\"/r/e.rs\",\"content\":\"line\"}"),
    );
    assert_eq!(edits(&stringly)[0].file, "/r/e.rs");

    // Not a tool step: nothing.
    let model = span(
        "t",
        "l",
        1,
        json!({"gen_ai.operation.name": "chat",
        "gen_ai.tool.call.arguments": {"file_path": "/r/f.rs", "content": "x"}}),
    );
    assert!(edits(&model).is_empty());
}

#[test]
fn codex_file_change_items_are_read_as_edits() {
    let change = tool_span(
        "t",
        "f",
        1,
        "apply_patch",
        json!({"changes": {
            "/r/new.md": {"type": "add", "content": "# Title\nbody\n"},
            "src/app.py": {"type": "update", "move_path": null,
                "unified_diff": "--- a/src/app.py\n+++ b/src/app.py\n@@ -1,2 +1,2 @@\n-old()\n+new()\n same()\n"},
            "/r/gone.py": {"type": "delete"},
        }}),
    );
    let mut read = edits(&change);
    read.sort_by(|a, b| a.file.cmp(&b.file));
    let files: Vec<&str> = read.iter().map(|edit| edit.file.as_str()).collect();
    assert_eq!(
        files,
        ["/r/new.md", "src/app.py"],
        "a deletion adds nothing"
    );
    assert_eq!(read[0].added, ["# Title", "body"]);
    assert_eq!(read[1].added, ["new()"], "file headers are not lines");
    assert_eq!(read[1].removed, ["old()"]);
}

#[test]
fn codex_patches_are_parsed_per_file_and_joined_with_the_working_directory() {
    let patch = "*** Begin Patch\n*** Add File: docs/new.md\n+# Title\n+body\n\
                 *** Update File: src/app.py\n@@ def main():\n-    old()\n+    new()\n     same()\n\
                 *** Update File: src/old_name.py\n*** Move to: src/new_name.py\n@@\n+moved = True\n\
                 *** Delete File: src/gone.py\n*** End Patch\n";
    let parsed = parse_patch(patch);
    let files: Vec<&str> = parsed.iter().map(|edit| edit.file.as_str()).collect();
    assert_eq!(files, ["docs/new.md", "src/app.py", "src/new_name.py"]);
    assert_eq!(parsed[1].added, ["    new()"]);
    assert_eq!(parsed[1].removed, ["    old()"]);

    let mut apply = tool_span("t", "p", 1, "apply_patch", json!(patch));
    apply
        .attributes
        .insert("code.cwd".into(), json!("/work/api"));
    let read = edits(&apply);
    assert_eq!(read[0].file, "/work/api/docs/new.md");
    assert!(read[1].introduces("new()"));

    // The oldest Codex shape: apply_patch through a shell command array.
    let shell = tool_span(
        "t",
        "s",
        1,
        "shell",
        json!({"command": ["apply_patch", patch], "workdir": "/w"}),
    );
    assert_eq!(edits(&shell)[2].file, "/w/src/new_name.py");
}

fn blame_store(label: &str) -> TempStore {
    let fixture = TempStore::open(label, crate::Config::default());
    let mut turn = span("t1", "turn", 100, json!({"session.id": "run-1"}));
    turn.events.push(crate::Event {
        name: "gen_ai.user.message".into(),
        timestamp_ns: 100,
        attributes: json!({"content": "add a total"})
            .as_object()
            .cloned()
            .unwrap_or_default(),
    });
    let mut call = span(
        "t1",
        "call",
        110,
        json!({"session.id": "run-1", "gen_ai.operation.name": "chat",
               "gen_ai.request.model": "model-a"}),
    );
    call.parent_span_id = Some("turn".into());
    let mut first = tool_span(
        "t1",
        "edit-1",
        120,
        "Edit",
        json!({"file_path": "/repo/src/cart.rs", "old_string": "", "new_string": "    pub fn total(&self) -> u64 { 0 }"}),
    );
    first.parent_span_id = Some("call".into());
    first.attributes.insert("session.id".into(), json!("run-1"));
    // Later, a second session writes the same line again.
    let mut later = tool_span(
        "t2",
        "edit-2",
        900,
        "Write",
        json!({"file_path": "/other/src/cart.rs", "content": "pub fn total(&self) -> u64 { 0 }\n"}),
    );
    later.attributes.insert("session.id".into(), json!("run-2"));
    // An edit that only REMOVES the line is not an introduction.
    let mut removal = tool_span(
        "t1",
        "edit-0",
        50,
        "Edit",
        json!({"file_path": "/repo/src/cart.rs", "old_string": "pub fn total(&self) -> u64 { 0 }", "new_string": ""}),
    );
    removal
        .attributes
        .insert("session.id".into(), json!("run-1"));
    // Another tenant's identical edit.
    let mut foreign = tool_span(
        "t9",
        "edit-9",
        10,
        "Edit",
        json!({"file_path": "/repo/src/cart.rs", "old_string": "", "new_string": "pub fn total(&self) -> u64 { 0 }"}),
    );
    foreign.tenant = "acme".into();
    fixture
        .store()
        .ingest_batch(vec![turn, call, first, later, removal, foreign])
        .expect("ingests");
    fixture
}

#[test]
fn blame_names_the_earliest_introduction_with_its_prompt_model_and_step() {
    let fixture = blame_store("earliest");
    let blame = fixture
        .store()
        .blame(
            Some(""),
            &anywhere("src/cart.rs"),
            "pub fn total(&self) -> u64 { 0 }",
            5,
        )
        .expect("blames");
    let ids: Vec<&str> = blame.matches.iter().map(|m| m.span_id.as_str()).collect();
    assert_eq!(
        ids,
        ["edit-1", "edit-2"],
        "earliest first; the removal is not one"
    );
    let first = &blame.matches[0];
    assert_eq!(first.run, "run-1");
    assert_eq!(first.tool, "Edit");
    assert_eq!(first.file, "/repo/src/cart.rs");
    assert_eq!(first.prompt.as_deref(), Some("add a total"));
    assert_eq!(first.model.as_deref(), Some("model-a"));
    assert_eq!(first.step_index, Some(2), "removal, call, then this edit");
    assert_eq!(first.at_ns, 120);
    assert!(blame.searched_spans >= 3);
    assert!(!blame.truncated);

    let one = fixture
        .store()
        .blame(
            Some(""),
            &anywhere("cart.rs"),
            "pub fn total(&self) -> u64 { 0 }",
            1,
        )
        .expect("blames");
    assert_eq!(one.matches.len(), 1);
    assert_eq!(
        one.matches[0].span_id, "edit-1",
        "the limit keeps the earliest"
    );
}

#[test]
fn blame_is_tenant_scoped() {
    let fixture = blame_store("tenancy");
    let line = "pub fn total(&self) -> u64 { 0 }";
    let acme = fixture
        .store()
        .blame(Some("acme"), &anywhere("src/cart.rs"), line, 5)
        .expect("blames");
    assert_eq!(acme.matches.len(), 1);
    assert_eq!(acme.matches[0].tenant, "acme");
    let default = fixture
        .store()
        .blame(Some(""), &anywhere("src/cart.rs"), line, 5)
        .expect("blames");
    assert!(default.matches.iter().all(|m| m.tenant.is_empty()));
    let operator = fixture
        .store()
        .blame(None, &anywhere("src/cart.rs"), line, 5)
        .expect("blames");
    assert_eq!(operator.matches.len(), 3);
    assert_eq!(
        operator.matches[0].tenant, "acme",
        "the earliest, across tenants"
    );
}

#[test]
fn blame_refuses_what_it_cannot_search_and_honours_the_deadline() {
    let fixture = blame_store("refusals");
    let store = fixture.store();
    for (path, line) in [("src/cart.rs", "   "), ("", "x = 1"), ("/", "x = 1")] {
        match store.blame(Some(""), &anywhere(path), line, 5) {
            Err(Error::Invalid(_)) => {}
            other => panic!("{path:?} {line:?}: expected a refusal, got {other:?}"),
        }
    }
    // Any line with text can be asked about — punctuation, non-ASCII — since
    // candidates come from the tool-name index, not from words.
    for line in ["}", "日本語のコメント"] {
        let answer = store
            .blame(Some(""), &anywhere("src/cart.rs"), line, 5)
            .expect("answers");
        assert!(answer.matches.is_empty());
        assert_eq!(
            answer.searched_spans, 3,
            "the default tenant's three edit calls, and only them"
        );
    }

    let config = crate::Config {
        query_deadline: Some(std::time::Duration::ZERO),
        ..crate::Config::default()
    };
    let expired = TempStore::open("blame-deadline", config);
    match expired
        .store()
        .blame(Some(""), &anywhere("a.rs"), "let total = 1;", 5)
    {
        Err(Error::DeadlineExceeded(_)) => {}
        other => panic!("a spent budget must refuse, got {other:?}"),
    }
}

#[test]
fn a_project_scope_anchors_the_path_at_each_repository_root() {
    let scope = BlameScope::new(
        "src/main.rs",
        &["/home/me/app".to_owned(), "/home/me/app-wt".to_owned()],
    )
    .expect("valid");
    assert!(scope.matches("/home/me/app/src/main.rs", None));
    assert!(
        scope.matches("/home/me/app-wt/src/main.rs", None),
        "a worktree of the same repository"
    );
    assert!(
        !scope.matches("/home/other/src/main.rs", None),
        "another repository's src/main.rs is not this file"
    );
    assert!(
        !scope.matches("/home/me/app/crates/x/src/main.rs", None),
        "nor is another src/main.rs in the same repository"
    );
    assert!(
        !scope.matches("/home/me/application/src/main.rs", None),
        "a root is a whole component"
    );
    // A recorded path that stayed relative is placed with the session's
    // project; without one it cannot be said to be here.
    assert!(scope.matches("src/main.rs", Some("/home/me/app")));
    assert!(!scope.matches("src/main.rs", Some("/home/other")));
    assert!(!scope.matches("src/main.rs", None));
    // An absolute query path must lie under a root.
    let absolute =
        BlameScope::new("/home/me/app/src/main.rs", &["/home/me/app".to_owned()]).expect("valid");
    assert!(absolute.matches("/home/me/app/src/main.rs", None));
    assert!(!absolute.matches("/x/home/me/app/src/main.rs", None));
    // Without projects: the suffix match across every project, as before.
    let open = BlameScope::new("src/main.rs", &[]).expect("valid");
    assert!(open.matches("/home/other/src/main.rs", None));

    // A directory of checkouts: any one directory directly under it.
    let nested = BlameScope::new(
        "src/main.rs",
        &["/home/me/app/.claude/worktrees/*".to_owned()],
    )
    .expect("valid");
    assert!(nested.matches("/home/me/app/.claude/worktrees/gone-now/src/main.rs", None));
    assert!(
        !nested.matches("/home/me/app/.claude/worktrees/a/b/src/main.rs", None),
        "exactly one level: a/b is not a checkout of its own"
    );
    assert!(!nested.matches("/home/me/app/.claude/worktrees/src/main.rs", None));
    assert!(!nested.matches("/home/me/app/src/main.rs", None));

    for bad in ["relative/dir", "/", "", "/*", "/a/*/b", "relative/*"] {
        assert!(
            matches!(
                BlameScope::new("a.rs", &[bad.to_owned()]),
                Err(Error::Invalid(_))
            ),
            "{bad:?} is not a repository root"
        );
    }
    let many: Vec<String> = (0..=MAX_BLAME_PROJECTS).map(|i| format!("/r{i}")).collect();
    assert!(matches!(
        BlameScope::new("a.rs", &many),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn blame_scoped_to_a_project_ignores_the_same_line_in_another_repository() {
    let fixture = blame_store("projects");
    let line = "pub fn total(&self) -> u64 { 0 }";
    let scoped = BlameScope::new("src/cart.rs", &["/other".to_owned()]).expect("valid");
    let answer = fixture
        .store()
        .blame(Some(""), &scoped, line, 5)
        .expect("blames");
    let ids: Vec<&str> = answer.matches.iter().map(|m| m.span_id.as_str()).collect();
    assert_eq!(
        ids,
        ["edit-2"],
        "only /other's edit, not /repo's earlier one"
    );
    assert_eq!(answer.projects, ["/other"]);
    let everywhere = fixture
        .store()
        .blame(Some(""), &anywhere("src/cart.rs"), line, 5)
        .expect("blames");
    assert_eq!(everywhere.matches.len(), 2);
}

#[test]
fn a_shell_call_is_a_candidate_only_when_marked_as_an_edit() {
    let fixture = TempStore::open("blame-shell", crate::Config::default());
    let patch = "*** Begin Patch\n*** Update File: src/a.py\n@@\n+def marked():\n*** End Patch\n";
    let mut marked = tool_span(
        "t",
        "marked",
        10,
        "shell",
        json!({"command": ["apply_patch", patch], "workdir": "svc"}),
    );
    marked.attributes.insert("code.cwd".into(), json!("/repo"));
    marked.attributes.insert(EDIT_MARKER.into(), json!(true));
    let unmarked = tool_span(
        "t",
        "unmarked",
        5,
        "shell",
        json!({"command": ["apply_patch", patch.replace("marked", "plain")]}),
    );
    let mut batch = vec![marked, unmarked];
    for index in 0..20 {
        batch.push(tool_span(
            "t",
            &format!("ls-{index}"),
            20 + index,
            "shell",
            json!({"command": ["ls"]}),
        ));
    }
    fixture.store().ingest_batch(batch).expect("ingests");
    let store = fixture.store();
    let answer = store
        .blame(Some(""), &anywhere("svc/src/a.py"), "def marked():", 5)
        .expect("blames");
    assert_eq!(answer.matches.len(), 1);
    assert_eq!(
        answer.matches[0].file, "/repo/svc/src/a.py",
        "joined with the call's own workdir, under the session's"
    );
    assert_eq!(
        answer.searched_spans, 1,
        "the marked call is the only candidate: plain shell commands are not read"
    );
    let plain = store
        .blame(Some(""), &anywhere("src/a.py"), "def plain():", 5)
        .expect("blames");
    assert!(
        plain.matches.is_empty(),
        "an unmarked shell call is not searched"
    );
}

#[test]
fn a_relative_patch_path_resolves_against_the_calls_workdir_first() {
    let patch = "*** Begin Patch\n*** Add File: src/a.py\n+x = 1\n*** End Patch\n";
    let mut call = tool_span(
        "t",
        "w",
        1,
        "shell",
        json!({"command": ["apply_patch", patch], "workdir": "/repo/svc"}),
    );
    call.attributes.insert("code.cwd".into(), json!("/repo"));
    assert_eq!(edits(&call)[0].file, "/repo/svc/src/a.py");
    // A relative workdir is under the session's directory.
    let mut relative = call.clone();
    relative.attributes.insert(
        "gen_ai.tool.call.arguments".into(),
        json!({"command": ["apply_patch", patch], "workdir": "svc"}),
    );
    assert_eq!(edits(&relative)[0].file, "/repo/svc/src/a.py");
    // No workdir: the session's.
    let mut bare = call.clone();
    bare.attributes.insert(
        "gen_ai.tool.call.arguments".into(),
        json!({"command": ["apply_patch", patch]}),
    );
    assert_eq!(edits(&bare)[0].file, "/repo/src/a.py");
}

#[test]
fn unified_diff_markers_are_read_by_hunk_not_by_prefix() {
    let diff = "--- a/run.sql\n+++ b/run.sql\n@@ -1,3 +1,3 @@\n-- old comment\n+-- new comment\n \
                select 1;\n---flag\n+++i;\n";
    let (added, removed) = parse_unified_diff(diff);
    assert_eq!(added, ["-- new comment", "++i;"]);
    assert_eq!(removed, ["- old comment", "--flag"]);

    // Two files back to back: the second header is a header again once the
    // first hunk's counted lines are done.
    let two =
        "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n--- a/y\n+++ b/y\n@@ -0,0 +1,2 @@\n+c\n+-- d\n";
    let (added, removed) = parse_unified_diff(two);
    assert_eq!(added, ["b", "c", "-- d"]);
    assert_eq!(removed, ["a"]);

    // A hunk header without counts holds until the next `diff` line.
    let open = "@@\n+++x\n---y\ndiff --git a/z b/z\n--- a/z\n+++ b/z\n";
    assert_eq!(
        parse_unified_diff(open),
        (vec!["++x".to_owned()], vec!["--y".to_owned()])
    );
    assert_eq!(hunk_counts("@@ -3,7 +3,9 @@ fn main()"), Some((7, 9)));
    assert_eq!(hunk_counts("@@ -3 +4 @@"), Some((1, 1)));
    assert_eq!(hunk_counts("@@"), None);
}

#[test]
fn hitting_the_candidate_cap_stops_the_search_and_says_truncated() {
    let fixture = TempStore::open("blame-cap", crate::Config::default());
    let store = fixture.store();
    // Three segments of edit calls.
    for segment in 0..3u64 {
        let batch: Vec<Span> = (0..5u64)
            .map(|index| {
                tool_span(
                    "t",
                    &format!("e-{segment}-{index}"),
                    segment * 100 + index,
                    "Edit",
                    json!({"file_path": "/r/a.rs", "old_string": "",
                           "new_string": format!("line {segment}-{index}")}),
                )
            })
            .collect();
        store.ingest_batch(batch).expect("ingests");
        store.flush().expect("flushes");
    }
    let capped = store
        .blame_capped(Some(""), &anywhere("a.rs"), "line 0-0", 5, 2)
        .expect("a capped search answers");
    assert!(capped.truncated);
    assert_eq!(capped.searched_spans, 2);
    let whole = store
        .blame_capped(Some(""), &anywhere("a.rs"), "line 0-0", 5, 15)
        .expect("answers");
    assert!(!whole.truncated, "exactly the cap is not truncation");
    assert_eq!(whole.searched_spans, 15);

    // The fold under it stops decoding when told to: one span visited, one
    // segment examined of three.
    let filter = SpanFilter {
        attributes: vec![("gen_ai.tool.name".into(), json!("Edit"))],
        ..SpanFilter::default()
    };
    let mut visited = 0;
    let cost = store
        .fold_spans_while(&filter, None, |_| {
            visited += 1;
            false
        })
        .expect("folds");
    assert_eq!(visited, 1);
    assert_eq!(
        cost.segments_examined, 1,
        "stopped inside the first segment"
    );
}
