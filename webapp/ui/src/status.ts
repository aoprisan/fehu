/**
 * The two status lines: one under the event form, one under the order ticket.
 * A tiny UI service so `actions.ts` can report the outcome of a request that
 * no panel is awaiting (a stream-driven refresh, say).
 */

import { byId } from './dom.js';

function write(id: string, message: string, isError: boolean): void {
  const el = byId(id);
  el.textContent = message;
  el.classList.toggle('err', isError);
}

export function setStatus(message: string, isError = false): void {
  write('status', message, isError);
}

export function setOrderStatus(message: string, isError = false): void {
  write('order-status', message, isError);
}
