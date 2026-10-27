import { runDaemon } from './daemon.ts';
import { runShim } from './shim.ts';

const cmd = process.argv[2];
if (cmd === 'daemon') {
  const port = process.env.AMM_PORT ? Number(process.env.AMM_PORT) : undefined;
  const d = await runDaemon({ port });
  if (!d) process.exit(0);
  console.log(`amm daemon pid ${process.pid} on 127.0.0.1:${d.port}`);
  for (const sig of ['SIGINT', 'SIGTERM'] as const) process.on(sig, () => void d.close().then(() => process.exit(0)));
} else if (cmd === 'shim') {
  await runShim();
} else {
  console.error('usage: amm.js daemon | shim');
  process.exit(2);
}
