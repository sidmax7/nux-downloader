// Thin layer over the generated bindings: a failed command throws an
// Error carrying the engine's message, instead of a `{ status }` result.

import { commands, type CollectOptionsDto } from "./bindings";

type Outcome<T> = { status: "ok"; data: T } | { status: "error"; error: string };

export async function unwrap<T>(call: Promise<Outcome<T>>): Promise<T> {
  const result = await call;
  if (result.status === "ok") return result.data;
  throw new Error(result.error);
}

export const api = {
  snapshot: () => unwrap(commands.engineSnapshot()),
  collect: (inputs: string[], options: CollectOptionsDto) =>
    unwrap(commands.collectLinks(inputs, options)),
  enqueue: (request: Parameters<typeof commands.enqueueDraft>[0]) =>
    unwrap(commands.enqueueDraft(request)),
  discardDrafts: () => unwrap(commands.discardDrafts()),
  pause: (id: number | null) => unwrap(commands.pause(id)),
  resume: (id: number | null) => unwrap(commands.resume(id)),
  remove: (id: number | null, deleteFiles: boolean) => unwrap(commands.remove(id, deleteFiles)),
  restartFile: (id: number) => unwrap(commands.restartFile(id)),
  setPriority: (id: number, priority: Parameters<typeof commands.setPriority>[1]) =>
    unwrap(commands.setPriority(id, priority)),
  move: (id: number, position: number) => unwrap(commands.movePackage(id, position)),
  setPostOptions: (id: number, extract: boolean, deleteArchives: boolean) =>
    unwrap(commands.setPostOptions(id, extract, deleteArchives)),
  setPackageLimit: (id: number, limit: number | null) =>
    unwrap(commands.setPackageLimit(id, limit)),
  blocks: (packageId: number, fileId: number) => unwrap(commands.fileBlocks(packageId, fileId)),
  getSettings: () => commands.getSettings(),
  downloadPath: () => commands.downloadPath(),
  updateSettings: (settings: Parameters<typeof commands.updateSettings>[0]) =>
    unwrap(commands.updateSettings(settings)),
  restartEngine: () => unwrap(commands.restartEngine()),
  pickFolder: () => commands.pickFolder(),
  diagnostics: () => unwrap(commands.diagnosticsReport()),
  about: () => commands.about(),
  checkForUpdate: () => unwrap(commands.checkForUpdate()),
  installUpdate: () => unwrap(commands.installUpdate()),
  notices: () => commands.thirdPartyNotices(),
};
