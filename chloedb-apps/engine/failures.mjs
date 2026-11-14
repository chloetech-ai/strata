// What a failed build says to the person who asked for it: one plain
// sentence of what went wrong and what to do, from the error as the model's
// CLI, the checks or the engine wrote it. The error itself stays, folded
// under "Details", for whoever runs the engine.

const KINDS = [
  // The model is not signed in where the engine runs.
  [/invalid api key|please run \/login|not logged in|log ?in required|authentication|unauthori[sz]ed|oauth token|\b401\b/i,
    'The model is not signed in where the app engine runs.', 'Whoever runs it signs Claude Code in there (claude auth login) or sets ANTHROPIC_API_KEY; then Try again.'],
  [/could not run .*claude|enoent/i,
    'The app engine could not start the model: Claude Code is not installed where it runs.', 'Whoever runs it installs Claude Code there and signs it in (or sets STRATA_APPS_CLAUDE); then Try again.'],
  [/spending limit|share of (its|this studio's) spending/i,
    'This studio has spent what it may on the model.', 'Whoever runs it raises the limit (STRATA_APPS_SPEND_CAP_USD); then Try again.'],
  [/usage limit|rate.?limit|too many requests|\b429\b|overloaded|\b529\b|quota|credit balance/i,
    'The model is busy or has reached its usage limit for now.', 'Try again in a few minutes.'],
  [/wrote nothing for|took longer than|timed? ?out/i,
    'The model stopped answering before the app was written.', 'Try again; it usually goes through the second time.'],
  [/did not pass the checks|held no <plan>|not valid json|held no json|does not start a part|create screen|the plan |does not parse|unknown block|is not a column|no such table|no table /i,
    'The model wrote something that did not pass the engine\'s checks, and its fixes did not either.', 'Try again, or say it in other words: a smaller first version helps.'],
];

export function explainFailure(message, { stopped = false } = {}) {
  if (stopped || /^stopped by its owner/.test(String(message || ''))) return { title: 'Stopped.', hint: 'Nothing changed. Try again when you want it built.' };
  const m = String(message || '');
  for (const [re, title, hint] of KINDS) if (re.test(m)) return { title, hint };
  return { title: 'The build did not go through.', hint: 'Try again; if it fails the same way, the details say why.' };
}
