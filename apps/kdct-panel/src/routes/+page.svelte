<script lang="ts">
  import { onMount } from 'svelte';

  let overview = $state<any>(null);
  let nodes = $state<any[]>([]);
  let images = $state<any[]>([]);
  let err = $state('');

  async function refresh() {
    try {
      const [o, n, i] = await Promise.all([
        fetch('/api/overview').then(r => r.json()),
        fetch('/api/nodes').then(r => r.json()),
        fetch('/api/images').then(r => r.json())
      ]);
      overview = o; nodes = n; images = i; err = '';
    } catch { err = 'Cannot reach kdcts server'; }
  }

  onMount(() => {
    refresh();
    const iv = setInterval(refresh, 4000);
    return () => clearInterval(iv);
  });

  function mem(mb: number) { return mb >= 1024 ? `${(mb/1024).toFixed(1)} GB` : `${mb} MB`; }
</script>

<div class="page-head">
  <h1>Overview</h1>
  <span class="page-head-sub">auto-refresh every 4s</span>
</div>

{#if err}
  <div class="msg err">{err}</div>
{/if}

{#if overview}
  <div class="stats">
    <div class="stat">
      <div class="stat-v">{overview.online_count}<span style="color:var(--text-dim);font-weight:400">/{overview.node_count}</span></div>
      <div class="stat-l">Nodes online</div>
    </div>
    <div class="stat">
      <div class="stat-v">{overview.configured_count}<span style="color:var(--text-dim);font-weight:400">/{overview.image_count}</span></div>
      <div class="stat-l">Images configured</div>
    </div>
    <div class="stat">
      <div class="stat-v">{overview.container_count}</div>
      <div class="stat-l">Containers running</div>
    </div>
  </div>

  <div class="section">
    <div class="section-head">
      <h2>Nodes</h2>
      <span>{nodes.length} total / {nodes.filter(n => n.status === 'online').length} online</span>
    </div>
    {#if nodes.length === 0}
      <div class="empty">No nodes registered. Start a client: <code>kdctc --config client.toml</code></div>
    {:else}
      <table>
        <thead><tr>
          <th></th><th>Hostname</th><th>OS / Arch</th><th>Docker</th><th>CPU</th><th>Memory</th><th>Ports</th>
        </tr></thead>
        <tbody>
          {#each nodes.slice(0, 8) as n}
            <tr>
              <td><span class="dot {n.status}"></span></td>
              <td class="hi">{n.hostname}</td>
              <td class="dim">{n.os || '—'} / {n.arch || '—'}</td>
              <td class="dim">{n.docker_version?.split('.')?.slice(0,2)?.join('.') || '—'}</td>
              <td class="dim">{n.cpu_cores} cores</td>
              <td class="dim">{mem(n.memory_mb)}</td>
              <td class="dim">{n.port_range_start}–{n.port_range_end}</td>
            </tr>
          {/each}
        </tbody>
      </table>
      {#if nodes.length > 8}
        <div style="text-align:center;padding:10px"><a href="/nodes" class="mono dim" style="font-size:11px">+{nodes.length - 8} more — view all</a></div>
      {/if}
    {/if}
  </div>

  <div class="section">
    <div class="section-head">
      <h2>Images</h2>
      <span>{images.length} loaded</span>
    </div>
    {#if images.length === 0}
      <div class="empty">No images loaded. Use: <code>kdcts image load &lt;source&gt;</code></div>
    {:else}
      <table>
        <thead><tr>
          <th>Name</th><th>Source</th><th>Type</th><th>Status</th>
        </tr></thead>
        <tbody>
          {#each images as img}
            <tr>
              <td class="hi">{img.name}</td>
              <td class="dim truncate" style="max-width:240px">{img.source}</td>
              <td class="dim">{img.source_type}</td>
              <td><span class="badge {img.status}">{img.status}</span></td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}
  </div>
{/if}
