<script lang="ts">
  import '../app.css';
  import { page } from '$app/state';
  let { children } = $props();

  const nav = [
    { href: '/',       label: 'Overview' },
    { href: '/nodes',  label: 'Nodes' },
    { href: '/images', label: 'Images' },
  ];
</script>

<div class="app">
  <header class="topbar">
    <a href="/" class="topbar-brand">
      <span class="topbar-brand-dot"></span>
      KDCT
    </a>
    <nav class="topbar-nav">
      {#each nav as item}
        <a href={item.href} class:active={page.url.pathname === item.href}>
          {item.label}
        </a>
      {/each}
    </nav>
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
