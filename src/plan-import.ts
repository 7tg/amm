export interface PlanTask {
  title: string;
  body: string;
  files: string[];
}

const TASK_HEADING = /^(#{2,4})\s+Task\s+\d+\s*[:.\-–—]?\s*(.*)$/i;
const ANY_HEADING = /^(#{1,6})\s/;
const FILE_PREFIX = /^\s*[-*]\s*(?:Create|Modify|Test|Update|Delete):\s*(.*)$/i;
const FENCE_OPEN = /^\s*(`{3,}|~{3,})/;

/** Splits a plan written as "### Task N: Title" sections (superpowers format) into tasks. */
export function parsePlan(md: string): PlanTask[] {
  const tasks: PlanTask[] = [];
  let cur: PlanTask | null = null;
  let level = 0;
  let fence: string | null = null;
  for (const line of md.split(/\r?\n/)) {
    if (fence) {
      const close = /^\s*(`{3,}|~{3,})\s*$/.exec(line);
      if (close && close[1][0] === fence[0] && close[1].length >= fence.length) fence = null;
    } else {
      const open = FENCE_OPEN.exec(line);
      if (open) fence = open[1];
    }
    const inFence = fence !== null;
    if (!inFence && !FENCE_OPEN.test(line)) {
      const heading = TASK_HEADING.exec(line);
      if (heading) {
        level = heading[1].length;
        cur = { title: heading[2].trim(), body: '', files: [] };
        tasks.push(cur);
        continue;
      }
      const any = ANY_HEADING.exec(line);
      if (any && any[1].length <= level) cur = null;
    }
    if (!cur) continue;
    cur.body += line + '\n';
    const prefix = inFence ? null : FILE_PREFIX.exec(line);
    if (prefix) {
      for (const m of prefix[1].matchAll(/`([^`]+)`/g)) cur.files.push(m[1].replace(/:\d+(-\d+)?$/, ''));
    }
  }
  for (const t of tasks) t.body = t.body.trim();
  return tasks;
}
