use crate::args::Args;
use crate::logger::Logger;
use crate::prompt;
use crate::run_mode::resolve_and_stream;
use crate::stream::StreamOutput;

const PLAN_SYSTEM_PROMPT: &str = "You are an implementation planner. The user will give you a task. Your job is to produce a clear, step-by-step implementation plan in Markdown.\n\nStrict rules:\n- Output ONLY the plan in Markdown. No greetings, no questions, no commentary.\n- Do not write or modify any files. Do not call any tools.\n- Use sections like \"## Goal\", \"## Approach\", \"## Steps\", \"## Risks\" as appropriate.\n- The plan will be reviewed by the user in an editor and then executed by another agent.";

/// Plan mode: 1) generate a plan with the plan-mode provider, 2) edit it in
/// `$EDITOR`, 3) re-resolve under build mode and execute.
///
/// `-m/--model <key>` overrides both the plan-mode key (default `"plan"`) and
/// the subsequent build-mode key (default `"build"`).
///
/// # Errors
///
/// Stringified errors from any stage (resolve/timeout/editor/build run).
pub fn run(
    rt: &tokio::runtime::Runtime,
    prompt: &str,
    args: &Args,
    logger: &Logger,
) -> Result<(), String> {
    let plan_key = args.model.as_deref().unwrap_or("plan");
    let build_key = args.model.as_deref().unwrap_or("build");

    // Fail fast if the editor cannot be safely opened, before paying for plan generation.
    prompt::ensure_editor_available().map_err(|e| e.to_string())?;

    // 1) generate the plan without streaming to stdout
    let plan_text = resolve_and_stream(
        rt,
        prompt,
        args,
        plan_key,
        Some(PLAN_SYSTEM_PROMPT),
        logger,
        StreamOutput::CaptureOnly,
    )?;

    // 2) edit in $EDITOR seeded with the captured plan
    let edited = prompt::edit_with_seed(&plan_text).map_err(|e| e.to_string())?;
    let trimmed = edited.trim();
    if trimmed.is_empty() {
        logger.info("Plan canceled");
        return Ok(());
    }

    // 3) re-resolve under build mode and stream the execution
    let build_prompt = format!("<plan>\n{trimmed}\n</plan>\n\nExecute the plan above.");
    resolve_and_stream(
        rt,
        &build_prompt,
        args,
        build_key,
        None,
        logger,
        StreamOutput::Forward,
    )?;
    Ok(())
}
