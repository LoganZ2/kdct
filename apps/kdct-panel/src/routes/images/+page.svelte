<script lang="ts">
  import { onMount } from 'svelte';

  interface ImageNode {
    id: number; name: string; source: string; source_type: string;
    status: string; created_at: number;
  }
  interface NodeItem { id: number; hostname: string; status: string; }
  interface Container {
    container_name: string; image: string; hostname: string;
    ports: number[]; status: string;
  }
  interface ImageDetail extends ImageNode {
    ports: { id: number; port: number; protocol: string; route_path: string | null }[];
    envs: { key: string; value: string }[];
  }

  let images = $state<ImageNode[]>([]);
  let nodes = $state<NodeItem[]>([]);
  let containers = $state<Container[]>([]);
  let err = $state('');

  let deployImg = $state<ImageNode | null>(null);
  let deployNid = $state(0);
  let deploying = $state(false);

  let detail = $state<ImageDetail | null>(null);

  let showLoad = $state(false);
  let loadSource = $state('');
  let loading = $state(false);
  let loadResult = $state<'ok' | 'err' | ''>('');
  let loadMsg = $state('');

  let searchQuery = $state('');
  let searchResults = $state<any[]>([]);
  let searching = $state(false);
  let searchTimer = 0;

  const online = $derived(nodes.filter(n => n.status === 'online'));

  async function refresh() {
    try {
      const [im, nd] = await Promise.all([
        fetch('/api/images').then(r => r.json()),
        fetch('/api/nodes').then(r => r.json())
      ]);
      images = im; nodes = nd; err = '';
      try { containers = await fetch('/api/deployments').then(r => r.json()); } catch { containers = []; }
    } catch { err = 'Cannot reach kdcts server'; }
  }

  onMount(() => {
    refresh();
    const iv = setInterval(refresh, 4000);
    return () => clearInterval(iv);
  });

  function deps(name: string) { return containers.filter(c => c.image === name || c.image.startsWith(name + ':')); }
  function nid(h: string) { return nodes.find(n => n.hostname === h)?.id || 0; }

  async function showDetail(img: ImageNode) {
    if (detail?.name === img.name) { detail = null; return; }
    detail = null;
    try { detail = await fetch(`/api/images/${encodeURIComponent(img.name)}`).then(r => r.json()); }
    catch { detail = null; }
  }

  function openDeploy(img: ImageNode) {
    deployImg = img;
    deployNid = online[0]?.id || 0;
  }

  async function doDeploy() {
    if (!deployImg || !deployNid) return;
    deploying = true;
    try {
      const res = await fetch('/api/deploy', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ image: deployImg.name, node_id: deployNid })
      });
      const t = await res.text();
      if (res.ok) { deployImg = null; refresh(); }
      else alert(`Error: ${t}`);
    } catch (e: any) { alert(e.message || 'Deploy failed'); }
    deploying = false;
  }

  async function doStop(name: string, hostname: string) {
    const id = nid(hostname); if (!id) return;
    try {
      const res = await fetch('/api/stop', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ image: name, node_id: id })
      });
      const t = await res.text();
      if (!res.ok) alert(`Error: ${t}`);
      refresh();
    } catch (e: any) { alert(e.message || 'Stop failed'); }
  }

  function openLoad() { showLoad = true; loadSource = ''; loadResult = ''; loadMsg = ''; searchQuery = ''; searchResults = []; }
  function closeLoad() { if (!loading) showLoad = false; }

  function handleSearchInput() {
    clearTimeout(searchTimer);
    if (searchQuery.length < 2) { searchResults = []; return; }
    searching = true;
    searchTimer = setTimeout(async () => {
      try {
        const res = await fetch(`/api/search?q=${encodeURIComponent(searchQuery)}`);
        searchResults = await res.json();
      } catch { searchResults = []; }
      searching = false;
    }, 250);
  }

  function pickImage(name: string) {
    loadSource = name;
    searchQuery = '';
    searchResults = [];
  }

  async function doLoad() {
    if (!loadSource.trim()) return;
    loading = true; loadResult = ''; loadMsg = '';
    try {
      const res = await fetch('/api/image/load', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ source: loadSource.trim() })
      });
      const t = await res.text();
      if (res.ok) { loadResult = 'ok'; loadMsg = t; }
      else { loadResult = 'err'; loadMsg = `Error: ${t}`; }
      refresh();
    } catch (e: any) { loadResult = 'err'; loadMsg = e.message || 'Load failed'; }
    loading = false;
  }
</script>

<div class="page-head">
  <h1>Images</h1>
  <div style="display:flex;align-items:center;gap:12px">
    <span class="page-head-sub">{images.length} loaded &middot; {containers.length} containers running &middot; auto-refresh 4s</span>
    <button class="primary" style="font-size:11px;padding:4px 10px" onclick={openLoad}>+ Load Image</button>
  </div>
</div>

{#if err}
  <div class="msg err">{err}</div>
{/if}

{#if images.length === 0}
  <div class="empty">No images loaded. Use <code>kdcts image load &lt;source&gt;</code> or click <em>+ Load Image</em> above.</div>
{:else}
  <table>
    <thead><tr>
      <th>Name</th><th>Source</th><th>Type</th><th>Status</th><th>Running on</th><th></th>
    </tr></thead>
    <tbody>
      {#each images as img}
        {@const dd = deps(img.name)}
        <tr>
          <td class="hi" style="cursor:pointer;font-weight:500" onclick={() => showDetail(img)} onkeydown={(e) => { if (e.key === 'Enter') showDetail(img); }} role="button" tabindex="0">{img.name}</td>
          <td class="dim truncate" style="max-width:200px">{img.source}</td>
          <td class="dim">{img.source_type}</td>
          <td><span class="badge {img.status}">{img.status}</span></td>
          <td>
            {#if dd.length > 0}
              {#each dd as c}
                <div style="display:flex;align-items:center;gap:5px;padding:1px 0">
                  <span class="dot online" style="width:5px;height:5px"></span>
                  <span class="dim" style="font-size:11px">{c.hostname}</span>
                  <span class="dim" style="font-size:11px">&middot; :{c.ports?.join(',') || '—'}</span>
                </div>
              {/each}
            {:else}
              <span class="dim">—</span>
            {/if}
          </td>
          <td>
            <div style="display:flex;gap:6px">
              {#if img.status === 'configured' && online.length > 0}
                <button onclick={() => openDeploy(img)}>Deploy</button>
              {/if}
              {#each dd as c}
                <button class="danger" onclick={() => doStop(img.name, c.hostname)}>Stop</button>
              {/each}
            </div>
          </td>
        </tr>
        {#if detail?.name === img.name}
          <tr>
            <td colspan="6" style="padding:0;border-bottom:1px solid var(--border2)">
              <div class="expand-pad">
                <div class="cols">
                  <div>
                    <div class="section-head" style="margin-bottom:8px"><h2>Ports & Routes</h2></div>
                    {#if detail.ports?.length}
                      <table><thead><tr><th>Port</th><th>Proto</th><th>Route Path</th></tr></thead>
                        <tbody>{#each detail.ports as p}
                          <tr>
                            <td class="hi">{p.port}</td>
                            <td class="dim">{p.protocol}</td>
                            <td class="dim">{p.route_path || '(not set)'}</td>
                          </tr>
                        {/each}</tbody>
                      </table>
                    {:else}
                      <div class="dim mono" style="font-size:12px;padding:8px 0">No ports configured</div>
                    {/if}
                  </div>
                  <div>
                    <div class="section-head" style="margin-bottom:8px"><h2>Environment</h2></div>
                    {#if detail.envs?.length}
                      <table><thead><tr><th>Key</th><th>Value</th></tr></thead>
                        <tbody>{#each detail.envs as e}
                          <tr>
                            <td class="hi">{e.key}</td>
                            <td class="dim">{e.value}</td>
                          </tr>
                        {/each}</tbody>
                      </table>
                    {:else}
                      <div class="dim mono" style="font-size:12px;padding:8px 0">No environment variables</div>
                    {/if}
                  </div>
                </div>
              </div>
            </td>
          </tr>
        {/if}
      {/each}
    </tbody>
  </table>
{/if}

{#if deployImg}
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <div class="overlay" onclick={() => deployImg = null} onkeydown={(e) => { if (e.key === 'Escape') deployImg = null; }}>
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div class="modal" onclick={(e) => e.stopPropagation()} onkeydown={() => {}}>
      <div class="modal-head">
        <span>Deploy <em>{deployImg.name}</em></span>
        <button class="ghost" onclick={() => deployImg = null}>Cancel</button>
      </div>
      {#if online.length === 0}
        <div class="empty">No online nodes available</div>
      {:else}
        <div class="field">
          <label for="dn">Target node</label>
          <select id="dn" bind:value={deployNid}>
            {#each online as n}
              <option value={n.id}>{n.hostname} (id {n.id})</option>
            {/each}
          </select>
        </div>
        <button class="primary" style="width:100%" onclick={doDeploy} disabled={deploying || !deployNid}>
          {deploying ? 'Deploying...' : 'Deploy'}
        </button>
      {/if}
    </div>
  </div>
{/if}

{#if showLoad}
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <div class="overlay" onclick={closeLoad} onkeydown={(e) => { if (e.key === 'Escape') closeLoad(); }}>
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div class="modal" onclick={(e) => e.stopPropagation()} onkeydown={() => {}}>
      <div class="modal-head">
        <span>Load <em>Image</em></span>
        <button class="ghost" onclick={closeLoad} disabled={loading}>Close</button>
      </div>

      {#if loadResult === 'ok'}
        <div class="msg ok">{loadMsg}</div>
        <button class="ghost" style="width:100%;margin-top:12px" onclick={() => showLoad = false}>Close</button>
      {:else if loadResult === 'err'}
        <div class="msg err">{loadMsg}</div>
        <button class="ghost" style="width:100%;margin-top:12px" onclick={() => { loadResult = ''; loadMsg = ''; }}>Try again</button>
      {:else}
        <div class="field">
          <label for="search">Search Docker Hub</label>
          <input id="search" bind:value={searchQuery} oninput={handleSearchInput} placeholder="nginx, redis, postgres..." disabled={loading || loadSource !== ''} />
        </div>
        {#if searching}
          <div class="dim" style="font-size:11px;padding:8px 0">Searching...</div>
        {/if}
        {#if searchResults.length > 0}
          <div class="search-list">
            {#each searchResults as r}
              <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
              <div class="search-item" onclick={() => pickImage(r.name)} onkeydown={(e) => { if (e.key === 'Enter') pickImage(r.name); }} role="button" tabindex="0">
                <div>
                  <span class="hi">{r.name}</span>
                  {#if r.is_official}<span class="badge online">OFFICIAL</span>{/if}
                </div>
                <div class="dim" style="font-size:10px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">{r.description || '—'}</div>
              </div>
            {/each}
          </div>
        {/if}
        {#if searchQuery.length >= 2 && searchResults.length === 0 && !searching}
          <div class="dim" style="font-size:11px;padding:8px 0">No results. Type a full name below.</div>
        {/if}
        <div class="field">
          <label for="src">Source</label>
          <input id="src" bind:value={loadSource} placeholder="nginx:latest  or  https://github.com/user/repo.git" disabled={loading} />
          <div class="dim" style="font-size:10px;margin-top:4px">Docker Hub image name, or Git repository URL</div>
        </div>
        <button class="primary" style="width:100%" onclick={doLoad} disabled={loading || !loadSource.trim()}>
          {loading ? 'Loading...' : 'Load Image'}
        </button>
      {/if}
    </div>
  </div>
{/if}
