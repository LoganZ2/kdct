<script lang="ts">
  import '../app.css';
  let { children } = $props();
</script>

<div class="app">
  <header class="topbar">
    <span class="topbar-brand">
      <span class="topbar-brand-dot"></span>
      KDCT
    </span>
    <div class="topbar-spacer"></div>
    <div class="topbar-status">
      <span class="dot online"></span>
      server:9933
    </div>
  </header>
  <main class="main">
    {@render children()}
  </main>
</div>
