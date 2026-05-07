<script lang="ts">
  import { onMount } from 'svelte';

  interface Node {
    id: number; hostname: string; os: string; arch: string;
    docker_version: string; port_range_start: number; port_range_end: number;
    cpu_cores: number; memory_mb: number; status: string; last_seen: number;
  }

  let nodes = $state<Node[]>([]);
  let err = $state('');
  let sel = $state<Node | null>(null);

  async function refresh() {
    try { nodes = await fetch('/api/nodes').then(r => r.json()); err = ''; }
    catch { err = 'Cannot reach kdcts server'; }
  }

  onMount(() => {
    refresh();
    const iv = setInterval(refresh, 4000);
    return () => clearInterval(iv);
  });

  function mem(mb: number) { return mb >= 1024 ? `${(mb/1024).toFixed(1)} GB` : `${mb} MB`; }
  function dk(v: string) { return v ? v.split('.').slice(0,2).join('.') : '—'; }
  function ago(ts: number) {
    const s = Math.floor(Date.now()/1000 - ts);
    if (s < 60) return 'just now';
    if (s < 3600) return `${Math.floor(s/60)}m ago`;
    if (s < 86400) return `${Math.floor(s/3600)}h ago`;
    return `${Math.floor(s/86400)}d ago`;
  }
</script>

<div class="page-head">
  <h1>Nodes</h1>
  <span class="page-head-sub">{nodes.length} total &middot; {nodes.filter(n => n.status === 'online').length} online &middot; auto-refresh 4s</span>
</div>

{#if err}
  <div class="msg err">{err}</div>
{/if}

{#if nodes.length === 0}
  <div class="empty">No nodes registered. Launch a KDCT client on your target machines.</div>
{:else}
  <table>
    <thead><tr>
      <th></th><th>Hostname</th><th>OS</th><th>Arch</th><th>Docker</th><th>CPU</th><th>Memory</th><th>Port Range</th><th>Last Seen</th><th>Status</th>
    </tr></thead>
    <tbody>
      {#each nodes as node}
        <tr class="row-link" class:expanded={sel?.id === node.id}
            onclick={() => sel = sel?.id === node.id ? null : node}
            onkeydown={(e) => { if (e.key === 'Enter') sel = sel?.id === node.id ? null : node; }}
            role="button" tabindex="0">
          <td><span class="dot {node.status}"></span></td>
          <td class="hi" style="font-weight:500">{node.hostname}</td>
          <td class="dim">{node.os || '—'}</td>
          <td class="dim">{node.arch || '—'}</td>
          <td class="dim">{dk(node.docker_version)}</td>
          <td class="dim">{node.cpu_cores} cores</td>
          <td class="dim">{mem(node.memory_mb)}</td>
          <td class="dim">{node.port_range_start} &ndash; {node.port_range_end}</td>
          <td class="dim">{ago(node.last_seen)}</td>
          <td><span class="badge {node.status}">{node.status}</span></td>
        </tr>
      {/each}
    </tbody>
  </table>

  {#if sel}
    <div class="detail">
      <div class="detail-head">
        <h3>Node #{sel.id} &mdash; {sel.hostname}</h3>
        <button class="ghost" onclick={() => sel = null}>Close</button>
      </div>
      <div class="detail-body">
        <dl class="dl">
          <dt>Hostname</dt><dd>{sel.hostname}</dd>
          <dt>Operating System</dt><dd>{sel.os || '—'}</dd>
          <dt>Architecture</dt><dd>{sel.arch || '—'}</dd>
          <dt>Docker Version</dt><dd>{sel.docker_version || '—'}</dd>
          <dt>CPU Cores</dt><dd>{sel.cpu_cores}</dd>
          <dt>Memory</dt><dd>{mem(sel.memory_mb)}</dd>
          <dt>Port Range</dt><dd>{sel.port_range_start} &ndash; {sel.port_range_end}</dd>
          <dt>Status</dt><dd><span class="badge {sel.status}">{sel.status}</span></dd>
          <dt>Last Seen</dt><dd class="dim">{new Date(sel.last_seen * 1000).toLocaleString()}</dd>
        </dl>
      </div>
    </div>
  {/if}
{/if}
