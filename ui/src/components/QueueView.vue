<script setup lang="ts">
import { computed } from "vue";
import { useEngineStore } from "../stores/engine";
import PackageRow from "./PackageRow.vue";

const engine = useEngineStore();
const packages = computed(() => engine.packages);
</script>

<template>
  <section aria-label="Downloads">
    <p v-if="packages.length === 0" class="empty muted">
      Nothing here yet. Choose <strong>Add links</strong> or paste a link anywhere in this window.
    </p>
    <ul v-else class="list">
      <PackageRow
        v-for="(pkg, index) in packages"
        :key="pkg.id"
        :pkg="pkg"
        :index="index"
        :count="packages.length"
      />
    </ul>
  </section>
</template>

<style scoped>
.list {
  margin: 0;
  padding: 0;
  display: grid;
  gap: 8px;
}

.empty {
  text-align: center;
  padding: 48px 0;
}
</style>
