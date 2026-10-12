import { defineStore } from "pinia";
import { computed, shallowRef } from "vue";
import { api } from "../api";
import { events, type FileDto, type PackageDto, type SnapshotDto } from "../bindings";
import { useToastStore } from "./toasts";

const EMPTY: SnapshotDto = { packages: [], links: [], global_limit: null, engine_error: null };

export const useEngineStore = defineStore("engine", () => {
  const toasts = useToastStore();

  // Replaced whole on every update (4 Hz): Vue tracks the reference, not
  // every file inside it.
  const snapshot = shallowRef<SnapshotDto>(EMPTY);

  function apply(next: SnapshotDto) {
    snapshot.value = next;
  }

  async function start() {
    await events.snapshotEvent.listen((event) => apply(event.payload));
    await events.noticeEvent.listen((event) => {
      if (event.payload.level === "error") toasts.error(event.payload.message);
      else toasts.info(event.payload.message);
    });
    apply(await api.snapshot());
  }

  /** Runs a command and turns a failure into a toast. */
  async function run<T>(call: () => Promise<T>): Promise<T | undefined> {
    try {
      return await call();
    } catch (error) {
      toasts.error(error instanceof Error ? error.message : String(error));
      return undefined;
    }
  }

  const packages = computed(() => snapshot.value.packages);
  const links = computed(() => snapshot.value.links);
  // Speeds are measured by the engine over the last few seconds on its own
  // clock (Decision 34); the total is what the downloads are getting.
  const totalSpeed = computed(() => packages.value.reduce((sum, p) => sum + speedOf(p), 0));
  const hasRunning = computed(() =>
    packages.value.some((p) => p.state === "downloading" || p.state === "queued"),
  );
  const hasPaused = computed(() => packages.value.some((p) => p.state === "paused"));

  function speedOf(pkg: PackageDto): number {
    return pkg.state === "downloading" ? pkg.speed : 0;
  }

  function fileSpeedOf(file: FileDto): number {
    return file.state === "active" ? file.speed : 0;
  }

  return {
    snapshot,
    packages,
    links,
    totalSpeed,
    hasRunning,
    hasPaused,
    speedOf,
    fileSpeedOf,
    apply,
    start,
    run,
    pause: (id: number | null) => run(() => api.pause(id)),
    resume: (id: number | null) => run(() => api.resume(id)),
    remove: (id: number, deleteFiles: boolean) => run(() => api.remove(id, deleteFiles)),
    restartFile: (id: number) => run(() => api.restartFile(id)),
  };
});
