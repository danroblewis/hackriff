// Skeleton placeholders (T-149): each MUI panel slot gets an empty state naming the task that
// fills it. Each owning task replaces its `mount` import in main.ts with its real panel module.
import { h } from "./dom";
import type { MountFn } from "./context";

export function placeholder(title: string, task: string, note = ""): MountFn {
  return (el) => {
    el.replaceChildren(
      h("div", { class: "section-h" }, title),
      h("div", { class: "empty" }, `${note ? `${note} ` : ""}(${task})`),
    );
  };
}
