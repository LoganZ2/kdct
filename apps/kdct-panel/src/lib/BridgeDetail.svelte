<script lang="ts">
  import { createEventDispatcher } from 'svelte';

  let { bridgeId, detail, onlineNodes }: {
    bridgeId: number;
    detail: any;
    onlineNodes: any[];
  } = $props();

  let portContainerPort = $state(0);
  let portMode = $state('route');
  let portRoutePath = $state('');
  let portProtocols: string[] = $state(['tcp']);
  let addingPort = $state(false);
  let portMsg = $state('');

  let envKey = $state('');
  let envVal = $state('');
  let addingEnv = $state(false);
  let envMsg = $state('');

  const dispatch = createEventDispatcher();

  async function addPort() {
    if (!portContainerPort) return;
    if (portMode === 'route' && !portRoutePath.startsWith('/')) { portMsg = 'Path must start with /'; return; }
    portMsg = '';
    try {
      const body: any = { container_port: portContainerPort, mode: portMode };
      if (portMode === 'route') body.route_path = portRoutePath;
      if (portMode === 'direct') body.protocols = portProtocols;
      const res = await fetch(`/api/bridges/${bridgeId}/port`, {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
      });
      if (res.ok) {
        portContainerPort = 0; portRoutePath = ''; addingPort = false; portProtocols = ['tcp'];
        dispatch('refresh');
      } else { portMsg = await res.text(); }
    } catch (e: any) { portMsg = e.message; }
  }

  async function deletePort(containerPort: number) {
    await fetch(`/api/bridges/${bridgeId}/port/${containerPort}`, { method: 'DELETE' });
    dispatch('refresh');
  }

  async function addEnv() {
    if (!envKey) return;
    const cur = detail?.envs || [];
    const pairs = [...cur, { key: envKey, value: envVal }];
    envMsg = '';
    try {
      const res = await fetch(`/api/bridges/${bridgeId}/env`, {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ envs: pairs }),
      });
      if (res.ok) { envKey = ''; envVal = ''; addingEnv = false; dispatch('refresh'); }
      else { envMsg = await res.text(); }
    } catch (e: any) { envMsg = e.message; }
  }

  async function deleteEnv(key: string) {
    const cur = detail?.envs || [];
    const pairs = cur.filter((e: any) => e.key !== key);
    try {
      await fetch(`/api/bridges/${bridgeId}/env`, {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ envs: pairs }),
      });
      dispatch('refresh');
    } catch {}
  }
</script>

<div class="detail-panel">
  <!-- Ports -->
  <div class="section-head" style="margin-bottom:8px"><h3>Ports</h3></div>
  {#if detail?.ports?.length}
    <table style="margin-bottom:8px">
      <thead><tr><th>Container Port</th><th>Mode</th><th>Route Path</th><th>Protocols</th><th></th></tr></thead>
      <tbody>
        {#each detail.ports as p}
          <tr>
            <td class="hi">{p.container_port}</td>
            <td>{#if p.mode === 'direct'}<span class="badge direct">direct</span>{:else}<span class="badge route">route</span>{/if}</td>
            <td class="dim">{p.mode === 'route' ? (p.route_path || '-') : '-'}</td>
            <td class="dim">{p.mode === 'direct' ? (p.protocols || 'tcp') : 'http'}</td>
            <td><button class="ghost small danger" onclick={() => deletePort(p.container_port)}>×</button></td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
  {#if addingPort}
    <div class="config-row" style="margin-bottom:8px;flex-wrap:wrap">
      <input type="number" bind:value={portContainerPort} placeholder="Container port" style="width:60px" />
      <select bind:value={portMode} style="font-family:var(--mono);font-size:11px;background:var(--bg);border:1px solid var(--border2);color:var(--text-hi);padding:4px">
        <option value="route">route</option>
        <option value="direct">direct</option>
      </select>
      {#if portMode === 'route'}
        <input bind:value={portRoutePath} placeholder="/api/..." style="flex:1" />
      {:else}
        <label style="font-size:10px;display:flex;align-items:center;gap:2px"><input type="checkbox" bind:group={portProtocols} value="tcp" />TCP</label>
        <label style="font-size:10px;display:flex;align-items:center;gap:2px"><input type="checkbox" bind:group={portProtocols} value="udp" />UDP</label>
      {/if}
      <button class="ghost small" onclick={addPort}>Add</button>
      <button class="ghost small" onclick={() => addingPort = false}>Cancel</button>
    </div>
    {#if portMsg}<div class="dim" style="font-size:10px;color:var(--red)">{portMsg}</div>{/if}
  {:else}
    <button class="ghost small" onclick={() => { addingPort = true; portContainerPort = 0; portMode = 'route'; portRoutePath = ''; portProtocols = ['tcp']; portMsg = ''; }} style="margin-bottom:8px">+ Add Port</button>
  {/if}

  <!-- Envs -->
  <div class="section-head" style="margin-bottom:8px;margin-top:16px"><h3>Environment</h3></div>
  {#if detail?.envs?.length}
    <table style="margin-bottom:8px">
      <thead><tr><th>Key</th><th>Value</th><th></th></tr></thead>
      <tbody>
        {#each detail.envs as e}
          <tr><td class="hi">{e.key}</td><td class="dim">{e.value}</td><td><button class="ghost small danger" onclick={() => deleteEnv(e.key)}>×</button></td></tr>
        {/each}
      </tbody>
    </table>
  {/if}
  {#if addingEnv}
    <div class="config-row" style="margin-bottom:8px">
      <input bind:value={envKey} placeholder="KEY" style="flex:1" />
      <input bind:value={envVal} placeholder="VALUE" style="flex:2" />
      <button class="ghost small" onclick={addEnv}>Add</button>
      <button class="ghost small" onclick={() => addingEnv = false}>Cancel</button>
    </div>
    {#if envMsg}<div class="dim" style="font-size:10px;color:var(--red)">{envMsg}</div>{/if}
  {:else}
    <button class="ghost small" onclick={() => { addingEnv = true; envKey = ''; envVal = ''; envMsg = ''; }}>+ Add Env</button>
  {/if}

  <!-- Deploy info removed — connections handle deployment -->
</div>

