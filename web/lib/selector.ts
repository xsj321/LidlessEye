// Label selector composition and parsing.
//
// The structured filter rows and the `selector` query parameter are two views
// of the same thing: rows compose into `{app="api", env=~"prod-*"}` and a deep
// link parses back into rows. Whatever does not fit the row grammar is kept as
// a raw matcher so the filter still applies.

import type { LabelFilter } from "./types";

export const OPS: ReadonlyArray<readonly [string, string]> = [
  ["=", "is"],
  ["!=", "is not"],
  ["=~", "matches"],
  ["!~", "does not match"],
];

/** `{pod="x", service_name=~"api-*"}` — braces optional on the server. */
export function composeSelector(filters: LabelFilter[] | undefined): string {
  const parts: string[] = [];
  for (const filter of filters ?? []) {
    if (filter.raw) {
      // A matcher the UI could not break into row controls (pasted from a deep
      // link); it is kept verbatim so the filter still applies.
      parts.push(filter.raw);
      continue;
    }
    if (filter.label && filter.value && filter.op) {
      parts.push(`${filter.label}${filter.op}"${escapeValue(filter.value)}"`);
    }
  }
  return parts.join(",");
}

/** Parse a selector back into rows; whatever does not fit becomes a raw row. */
export function parseSelector(text: string | null | undefined): LabelFilter[] {
  const input = String(text ?? "").trim();
  if (!input) return [];
  const filters: LabelFilter[] = [];
  let rest = input;
  if (rest.startsWith("{") && rest.endsWith("}")) rest = rest.slice(1, -1);
  for (const part of splitTopLevel(rest)) {
    const matcher = /^([^\s=!~]+)\s*(=|!=|=~|!~)\s*"((?:[^"\\]|\\.)*)"$/.exec(part.trim());
    if (matcher) {
      filters.push({
        label: matcher[1],
        op: matcher[2],
        value: unescapeValue(matcher[3]),
      });
    } else {
      // Not a matcher we can edit as a row: keep it verbatim so a deep link
      // someone pasted still filters the same way.
      filters.push({ raw: part.trim() });
    }
  }
  return filters;
}

/** Split on commas that sit outside double quotes (the server's rule). */
function splitTopLevel(text: string): string[] {
  const parts: string[] = [];
  let current = "";
  let quoted = false;
  for (let i = 0; i < text.length; i += 1) {
    const c = text[i];
    if (quoted) {
      current += c;
      if (c === "\\") {
        i += 1;
        current += text[i] ?? "";
      } else if (c === '"') {
        quoted = false;
      }
    } else if (c === '"') {
      quoted = true;
      current += c;
    } else if (c === ",") {
      parts.push(current);
      current = "";
    } else {
      current += c;
    }
  }
  if (current.trim()) parts.push(current);
  return parts.filter((part) => part.trim().length > 0);
}

function escapeValue(value: string): string {
  return String(value).replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}

function unescapeValue(value: string): string {
  return String(value).replace(/\\(.)/g, "$1");
}
