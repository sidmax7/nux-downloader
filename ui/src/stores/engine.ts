import { defineStore } from "pinia";
import { computed, shallowRef } from "vue";
import { api } from "../api";
import { events, type PackageDto, type SnapshotDto } from "../bindings";
import { emptyTracker, trackSpeeds, type Tracker } from "../speed";
import { useToastStore } from "./toasts";

const EMPTY: SnapshotDto = { packages: [], links: [], global_limit: null, engine_error: null };

export const useEngineStore = defineStore("engine", () => {
  const toasts = useToastStore();

  // Replaced whole on every update (4 Hz): Vue tracks the reference, not
  // every file inside it.
  const snapshot = shallowRef<SnapshotDto>(EMPTY);
  const packageSpeeds = shallowRef<Map<number, number>>(new Map());
  const fileSpeeds = shallowRef<Map<number, number>>(new Map());
  let packageTracker: Tracker = emptyTracker();
  let fileTracker: Tracker = emptyTracker();

  function apply(next: SnapshotDto, now = Date.now()) {
    packageTracker = trackSpeeds(
      packageTracker,
      next.packages.map((p) => ({
        id: p.id,
        bytes: p.downloaded,
        active: p.state === "downloading",
      })),
      now,
    );
    fileTracker = trackSpeeds(
      fileTracker,
      next.packages.flatMap((p) =>
        p.files.map((f) => ({ id: f.id, bytes: f.downloaded, active: f.state === "active" })),
      ),
      now,
    );
    packageSpeeds.value = packageTracker.speeds;
    fileSpeeds.value = fileTracker.speeds;
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
  // A link's smoothed speed fades out slowly; with no connection it is 0.
  const totalSpeed = computed(() =>
    links.value.reduce((sum, link) => sum + (link.active > 0 ? link.speed : 0), 0),
  );
  const hasRunning = computed(() =>
    packages.value.some((p) => p.state === "downloading" || p.state === "queued"),
  );
  const hasPaused = computed(() => packages.value.some((p) => p.state === "paused"));

  function speedOf(pkg: PackageDto): number {
    return packageSpeeds.value.get(pkg.id) ?? 0;
  }

  return {
    snapshot,
    packages,
    links,
    totalSpeed,
    hasRunning,
    hasPaused,
    packageSpeeds,
    fileSpeeds,
    speedOf,
    apply,
    start,
    run,
    pause: (id: number | null) => run(() => api.pause(id)),
    resume: (id: number | null) => run(() => api.resume(id)),
    remove: (id: number, deleteFiles: boolean) => run(() => api.remove(id, deleteFiles)),
    restartFile: (id: number) => run(() => api.restartFile(id)),
  };
});
