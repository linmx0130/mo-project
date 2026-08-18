//! System prompt construction: a fixed harness preamble (mode-aware), the
//! global instructions + skills from the global agents dir (`MO_AGENTS_DIR`,
//! default `$HOME/.agents`), and the contents of `<workdir>/AGENTS.md`.
//!
//! The caller journals the returned prompt as a `SystemPrompt` event on the
//! session's first run; every later run reuses the journaled text verbatim,
//! so this function runs once per session (mode changes and `AGENTS.md`
//! edits mid-session never alter the system prompt).

use std::path::Path;

use mo_core::Mode;

/// The marker prefix the worker's history rebuild wraps a journaled handoff
/// in when it synthesizes the handoff's user-role message, so the model
/// understands that the handoff prompt *is* its memory of the task and that
/// it should continue from the handoff's next step.
pub const HANDOFF_USER_PREFIX: &str = "[Context compressed: the messages before this point were \
     summarized into the following handoff prompt and are no longer sent to the model (they \
     remain in the session journal for you to check). Treat the handoff prompt as your \
     authoritative memory of the task so far and continue from its next step.]\n\n";

/// The marker prefix the worker's history rebuild wraps a journaled
/// `AskUserAnswered` event in when it synthesizes the answers' user-role
/// message, so the model understands that the JSON object that follows is
/// the user's answer to the clarification question it asked via the
/// `ask_user` tool (the tool's "return value").
pub const ASK_USER_ANSWER_PREFIX: &str = "[The user answered your clarification question:]\n\n";

/// The marker prefix the worker's history rebuild wraps a journaled
/// `PermissionAnswered` event in when it synthesizes the decision's
/// user-role message, so the model understands that the text that follows
/// is the user's decision on the file-access permission request the harness
/// showed them (allowed → retry the tool call; denied → do not retry).
pub const PERMISSION_ANSWER_PREFIX: &str =
    "[The user decided a file-access permission request:]\n\n";

/// The user-role message the worker appends to the context when it asks the
/// model to generate a handoff prompt (context compression): the model
/// summarizes the whole conversation so far, and the result is journaled as
/// a `Handoff` event that future runs use as the compressed context's first
/// message.
pub fn handoff_instruction() -> String {
    "Context compression: the conversation history has reached the context-window \
     threshold, and the next turns will run from a compressed context that replaces all \
     earlier messages.\n\n\
     Write a handoff prompt that captures everything a fresh session needs to continue this \
     task without the earlier history. It must include:\n\
     1. The original user input (the task as the user gave it).\n\
     2. Facts about the environment you have learned: build/test commands, protocols, \
     ports, file layouts, tools, gotchas.\n\
     3. Key decisions made so far and the reasons behind them.\n\
     4. Current progress: what has been done and what is still in the todo list.\n\
     5. The next step to take.\n\n\
     Reply with ONLY the handoff prompt as plain text — no preamble, no code fences."
        .to_string()
}

/// Build the system prompt for a session's first run.
///
/// `mode` frames the agent's job (Build = full access, Plan = plan only,
/// Explore = investigate only) and `scratch` is the session scratch dir
/// (`<data_dir>/sessions/<id>/tmp`) where non-Build modes may create/edit/
/// remove files — the codebase stays read-only for them.
///
/// `forced_skills` are the skills the user force-loaded in the "New
/// session" form (`Session::skills`): their full `SKILL.md` contents are
/// inlined into the prompt so the model has them from the start (and they
/// are left out of the on-demand listing — no need to `load_skill` them).
/// A forced skill that no longer exists on disk is skipped defensively.
pub fn build_system_prompt(
    workdir: &Path,
    agents_dir: &Path,
    subagent_depth: u32,
    mode: Mode,
    scratch: &Path,
    forced_skills: &[String],
) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are an autonomous coding agent running inside the mo harness. \
         You complete tasks by reasoning and by calling the provided tools.\n\n",
    );
    prompt.push_str(&mode_framing(mode, workdir, scratch));
    prompt.push_str(&format!("Working directory: {}\n", workdir.display()));
    prompt.push_str(
        "Files inside the working directory are freely accessible. read_file may also \
         read the session scratch directory and global skill folders (see \"Global \
         skills\" below). Any other path requires the user's approval: the harness \
         shows a permission request in the UI, holds the call until the user decides, \
         and then returns the call's outcome (the file content, or a denial error) like \
         any other result. Never attempt to access files or run commands that escape \
         the working directory without approval.\n\n",
    );
    prompt.push_str(
        "The harness may compress the context mid-task when it nears the model's context \
         window: you will then receive a handoff prompt as a user message summarizing the \
         earlier conversation. Treat it as your memory of the task and continue.\n\n",
    );
    prompt.push_str(&tool_usage_rules(mode, scratch));
    if subagent_depth > 0 {
        prompt.push_str(&format!(
            "You are a subagent (nesting depth {}) spawned by another agent.\n\
             Work within your working directory, do not spawn further subagents unless\n\
             truly necessary, and return a concise summary of what you did and found.\n\n",
            subagent_depth
        ));
    }

    // Global agent instructions (`<agents_dir>/AGENTS.md`) — the broadest,
    // user-level rules that apply to every session, injected first.
    let global_agents_md = agents_dir.join("AGENTS.md");
    if let Ok(content) = std::fs::read_to_string(&global_agents_md)
        && !content.trim().is_empty()
    {
        prompt.push_str(&format!(
            "Global instructions from {}/AGENTS.md:\n{}\n\n",
            agents_dir.display(),
            content.trim()
        ));
    }

    // Global skills: `<agents_dir>/<skill>/SKILL.md` and
    // `<agents_dir>/skills/<skill>/SKILL.md`. Skills the user force-loaded
    // in the "New session" form (`forced_skills`) have their FULL SKILL.md
    // inlined right here — the model has them from the start and never
    // needs the `load_skill` tool for them. Every other skill is listed by
    // frontmatter name + description only; the model pulls the full
    // instructions on demand via the `load_skill` tool, which also returns
    // the skill folder path so `read_file` can reach bundled resources.
    let skills = mo_core::skills::discover_skills(agents_dir);
    let forced: Vec<String> = forced_skills
        .iter()
        .filter_map(|name| {
            let skill = mo_core::skills::find_skill(agents_dir, name)?;
            let content = std::fs::read_to_string(skill.path.join("SKILL.md")).ok()?;
            Some((skill, content))
        })
        .map(|(skill, content)| {
            format!(
                "### {}\nDescription: {}\n{}",
                skill.name,
                if skill.description.is_empty() {
                    "(no description)"
                } else {
                    &skill.description
                },
                content.trim()
            )
        })
        .collect();
    if !forced.is_empty() {
        prompt.push_str(
            "Forced skills (the user loaded these for this session — their full \
             instructions are part of this system prompt, so follow them and do not \
             call load_skill for them):\n",
        );
        for section in &forced {
            prompt.push_str(section);
            prompt.push_str("\n\n");
        }
    }
    // The on-demand listing: only the skills that are NOT already loaded.
    // When every skill is forced, the section (and its load_skill blurb)
    // is omitted entirely.
    let on_demand: Vec<&mo_core::skills::Skill> = skills
        .iter()
        .filter(|s| !forced_skills.iter().any(|f| f == &s.name))
        .collect();
    if !on_demand.is_empty() {
        prompt.push_str(&format!(
            "Global skills available from {} (load on demand):\n",
            agents_dir.display()
        ));
        for skill in &on_demand {
            prompt.push_str(&format!("### {}\n", skill.name));
            prompt.push_str(&format!(
                "Description: {}\n",
                if skill.description.is_empty() {
                    "(no description)"
                } else {
                    &skill.description
                }
            ));
        }
        prompt.push_str(
            "\nLoad a skill on demand by calling the load_skill tool with the \
             skill name: it returns the full SKILL.md instructions and the \
             absolute path of the skill's folder. read_file may read reference \
             files, scripts, and other resources inside skill folders.\n\n",
        );
    }

    let agents_md = workdir.join("AGENTS.md");
    if let Ok(content) = std::fs::read_to_string(&agents_md)
        && !content.trim().is_empty()
    {
        prompt.push_str(&format!(
            "Project instructions from {}/AGENTS.md:\n{}\n\n",
            workdir.display(),
            content.trim()
        ));
    }
    prompt
}

/// The mode-specific framing paragraph, right after the harness intro.
fn mode_framing(mode: Mode, workdir: &Path, scratch: &Path) -> String {
    match mode {
        Mode::Build => "You are in Build mode: you can modify the codebase (create/edit/remove \
             files), run commands, and use subagents and skills to get the job done.\n\
             If you need more input from the user to continue — a choice between approaches, \
             a preference, or a detail only they can decide — ask for it via the `ask_user` \
             tool: the question appears in the UI with the preset options and a free-text \
             input box, and the user's answer arrives as a user message.\n\n"
            .to_string(),
        Mode::Plan => format!(
            "You are in Plan mode. Your job is to produce a clear, actionable \
             implementation plan — do not implement anything yet.\n\
             The codebase ({}) is READ-ONLY: create/edit/remove are denied there.\n\
             You may create, edit and remove temporary files under the session scratch \
             directory {} (use absolute paths).\n\
             bash and bash_in_background are available but treat them as read-only (a soft \
             restriction): use them to gather facts (builds, tests, greps), not to change \
             anything.\n\
             If you need more input from the user to continue — a choice between \
             approaches, a preference, or a detail only they can decide — ask for it via \
             the `ask_user` tool: the question appears in the UI with the preset options \
             and a free-text input box, and the user's answer arrives as a user message.\n\
             Finish with the plan as your final answer, then take exactly one of two exits:\n\
             - No must-answer open questions: if the plan is complete and has no open \
             questions the user MUST answer before implementation (no ambiguous \
             requirements, no missing decisions, no choices only the user can make), call \
             the `request_mode_change` tool with mode \"build\" — message: a short summary \
             of the plan and what you will do once approved — and then stop. Do not ask \
             \"shall I proceed?\" in plain text: the mode-change request IS that question. \
             The user reviews the plan and approves or rejects in the UI; on approval the \
             session continues in build mode.\n\
             - Must-answer open questions: if the plan depends on answers only the user \
             can give, finish with the plan plus the questions listed, and do NOT call \
             `request_mode_change` — wait for the user's answers.\n\
             Decide details you can reasonably assume yourself and state the assumptions \
             in the plan; only genuinely blocking questions count as must-answer.\n\
             Do not try to work around the sandbox or use subagent to bypass.\n\n",
            workdir.display(),
            scratch.display()
        ),
        Mode::Explore => format!(
            "You are in Explore mode. Investigate the codebase to answer the user's \
             question or gather facts for a parent agent.\n\
             The codebase ({}) is READ-ONLY: create/edit/remove are denied there.\n\
             You may create, edit and remove temporary files under the session scratch \
             directory {} (use absolute paths).\n\
             Prefer read_file; run read-only bash or bash_in_background commands when \
             helpful.\n\
             If you need more input from the user to continue — a choice between \
             approaches, a preference, or a detail only they can decide — ask for it via \
             the `ask_user` tool: the question appears in the UI with the preset options \
             and a free-text input box, and the user's answer arrives as a user message.\n\
             If the task turns into modifying the codebase, call the `request_mode_change` \
             tool to ask the user to switch this session to build mode.\n\
             Report concise findings as your final answer.\n\n",
            workdir.display(),
            scratch.display()
        ),
    }
}

/// The tool-usage rules paragraph. Non-Build modes drop the create/edit
/// bullets in favour of the scratch-dir rule, so the model is never told to
/// do something the sandbox denies.
fn tool_usage_rules(mode: Mode, scratch: &Path) -> String {
    match mode {
        Mode::Build => "Tool usage rules:\n\
             - When you need information, read files first.\n\
             - Create new files with create_file (the parent directory must already\n\
               exist and the file must not exist); modify existing files with\n\
               edit_file.\n\
             - Make precise edits: provide a unique old_string that appears exactly once\n\
               (use replace_all only when every occurrence should change).\n\
             - A path outside the working directory (and outside the session scratch\n\
               dir) requires the user's approval: the harness shows a permission request\n\
               in the UI, holds the call until the user decides, and then returns the\n\
               call's outcome (the result, or a denial error) like any other result.\n\
             - Use bash for anything outside the file tools: builds, tests, git, etc.\n\
             - Use bash_in_background for commands that run longer than ~2 minutes:\n\
               it returns a process id immediately; check it with action=status and\n\
               stop it with action=kill. Redirect its output to a file (background\n\
               stdout/stderr are discarded).\n\
             - After making changes, verify them with read_file or bash before finishing.\n\
             - Tool calls in one message run concurrently (up to max_tool_concurrency);\n\
               completion order is not guaranteed, so never put dependent calls in the\n\
               same message — wait for a result, then make the next call.\n\
             - Report your final answer as a plain text message when no more tool calls\n\
               are needed.\n\n"
            .to_string(),
        _ => format!(
            "Tool usage rules:\n\
             - When you need information, read files first.\n\
             - create_file / edit_file / remove_file are allowed ONLY under the session\n\
               scratch directory {} (absolute paths); the codebase is read-only and\n\
               modifications there are denied.\n\
             - Reading a path outside the working directory and the scratch dir shows\n\
               a permission request to the user in the UI (the call is held until the\n\
               user decides, then returns its outcome); writing outside the scratch dir\n\
               is denied outright — never asked about.\n\
             - Use bash or bash_in_background for anything outside the file tools\n\
               (builds, tests, git, ...), keeping them read-only.\n\
             - Tool calls in one message run concurrently (up to max_tool_concurrency);\n\
               completion order is not guaranteed, so never put dependent calls in the\n\
               same message — wait for a result, then make the next call.\n\
             - Report your final answer as a plain text message when no more tool calls\n\
               are needed.\n\n",
            scratch.display()
        ),
    }
}

// Unit tests live in `mo_worker/src/tests/prompt_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "tests/prompt_tests.rs"]
mod tests;
