// HTTP Cloud Function, triggered daily by Cloud Scheduler. Responds 500 on anything that
// needs a human, so the Scheduler job fails and the ERROR log entry can alert.
import { http } from '@google-cloud/functions-framework';
import { loadConfig, runKeeper } from './keeper.js';
import * as log from './log.js';

http('keeper', async (_req, res) => {
  try {
    res.json(await runKeeper(loadConfig(), log.info));
  } catch (e) {
    const message = `keeper: ${(e as Error).message}`;
    log.error(message);
    res.status(500).send(message);
  }
});
