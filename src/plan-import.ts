export interface PlanTask {
  title: string;
  body: string;
  files: string[];
}

const TASK_HEADING = /^#{2,4}\s+Task\s+\d+\s*[:.\-–]?\s*(.*)$/i;
const FILE_LINE = /^\s*[-*]\s*(?:Create|Modify|Test|Update|Delete):\s*`([^`]+)`/i;
const FENCE = /^\s*(```|~~~)/;

/** Splits a plan written as "### Task N: Title" sections (superpowers format) into tasks. */
export function parsePlan(md: string): PlanTask[] {
  const tasks: PlanTask[] = [];
  let cur: PlanTask | null = null;
  let inFence = false;
  for (const line of md.split('\n')) {
    if (FENCE.test(line)) inFence = !inFence;
    const heading = inFence ? null : TASK_HEADING.exec(line);
    if (heading) {
      cur = { title: heading[1].trim(), body: '', files: [] };
      tasks.push(cur);
      continue;
    }
    if (!cur) continue;
    cur.body += line + '\n';
    const file = inFence ? null : FILE_LINE.exec(line);
    if (file) cur.files.push(file[1].replace(/:\d+(-\d+)?$/, ''));
  }
  for (const t of tasks) t.body = t.body.trim();
  return tasks;
}
