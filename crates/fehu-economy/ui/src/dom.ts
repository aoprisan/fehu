/** Minimal typed DOM helpers. No framework: views are functions of state. */

/**
 * Look up a required element by id, checked against the expected type.
 * Throws at boot rather than failing with `null` deep inside a render.
 */
export function byId<T extends Element = HTMLElement>(
  id: string,
  ctor: abstract new (...args: never[]) => T = HTMLElement as unknown as abstract new (
    ...args: never[]
  ) => T,
): T {
  const el = document.getElementById(id);
  if (el === null) throw new Error(`missing element #${id}`);
  if (!(el instanceof ctor)) {
    throw new Error(`#${id} is a ${el.constructor.name}, expected ${ctor.name}`);
  }
  return el;
}

/** Query a required descendant, checked against the expected type. */
export function query<T extends Element>(
  root: ParentNode,
  selector: string,
  ctor: abstract new (...args: never[]) => T,
): T {
  const el = root.querySelector(selector);
  if (el === null) throw new Error(`missing element ${selector}`);
  if (!(el instanceof ctor)) {
    throw new Error(`${selector} is a ${el.constructor.name}, expected ${ctor.name}`);
  }
  return el;
}

export function queryAll<T extends Element>(root: ParentNode, selector: string): T[] {
  return Array.from(root.querySelectorAll<T>(selector));
}

type Attrs = Record<string, string | number | boolean | undefined>;
type Child = Node | string | null | undefined | false;

/**
 * Create an element. Attributes are set as attributes (not properties), so
 * `class`, `data-*` and ARIA all work; `false`/`undefined` values are skipped.
 */
export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Attrs = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === false) continue;
    node.setAttribute(k, v === true ? '' : String(v));
  }
  append(node, children);
  return node;
}

export function append(parent: Node, children: Child[]): void {
  for (const c of children) {
    if (c === null || c === undefined || c === false) continue;
    parent.appendChild(typeof c === 'string' ? document.createTextNode(c) : c);
  }
}

/** A `<td>`, the unit these tables are built from. */
export function td(className: string, ...children: Child[]): HTMLTableCellElement {
  return el('td', { class: className }, ...children);
}

export function clear(node: Element): void {
  node.replaceChildren();
}

/** Replace a container's children in one shot — no innerHTML, no reflow churn. */
export function replace(node: Element, children: Child[]): void {
  const frag = document.createDocumentFragment();
  append(frag, children);
  node.replaceChildren(frag);
}
