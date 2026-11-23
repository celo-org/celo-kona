// Runs the keeper once from a shell, configured like the Cloud Function. Prints the result
// as JSON on the last line and exits non-zero on anything that needs a human.
//
// Usage: node dist/cli.js [--dry-run]
import { loadConfig, runKeeper } from './keeper.js';
import * as log from './log.js';

try {
  if (process.argv.includes('--dry-run')) process.env.DRY_RUN = 'true';
  console.log(JSON.stringify(await runKeeper(loadConfig(), log.info)));
} catch (e) {
  log.error(`keeper: ${(e as Error).message}`);
  process.exitCode = 1;
}
