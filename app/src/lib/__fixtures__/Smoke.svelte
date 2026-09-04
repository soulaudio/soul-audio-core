<script lang="ts">
  let { label = 'smoke' }: { label?: string } = $props();
  let n = $state(0);
</script>

<button onclick={() => (n += 1)}>{label}: {n}</button>
