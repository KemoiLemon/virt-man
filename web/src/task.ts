// Task log: long daemon operations (create VM, ISO upload) register here and
// advance through running -> done/failed. Rendered by ui.ts as a bottom dock.

import type { Task } from "./api";

let nextId = 1;
const tasks: Task[] = [];
const listeners = new Set<() => void>();

export function onTasks(fn: () => void) {
  listeners.add(fn);
}

function emit() {
  for (const fn of listeners) fn();
}

export function allTasks(): Task[] {
  return tasks.slice();
}

export function runTask(label: string, work: (set: (msg: string) => void) => Promise<void>) {
  const task: Task = { id: nextId++, label, status: "running", message: "…" };
  tasks.unshift(task);
  emit();
  work((msg) => {
    task.message = msg;
    emit();
  })
    .then(() => {
      task.status = "done";
      emit();
    })
    .catch((e) => {
      task.status = "failed";
      task.message = e instanceof Error ? e.message : String(e);
      emit();
    });
  return task;
}

export function clearTask(id: number) {
  const i = tasks.findIndex((t) => t.id === id);
  if (i >= 0) tasks.splice(i, 1);
  emit();
}
