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
  let loading = $state(false);
  let loadJobId = $state('');
  let loadLogs = $state<string[]>([]);
  let loadResult = $state<'ok' | 'err' | ''>('');
  let loadMsg = $state('');

  let searchQuery = $state('');
  let searchResults = $state<any[]>([]);
  let searching = $state(false);
  let searchTimer = 0;

  let selectedRepo = $state('');
  let selectedTag = $state('');
  let tags = $state<string[]>([]);
  let tagPage = $state(1);
  let hasMoreTags = $state(false);
  let loadingTags = $state(false);
  let tagFilter = $state('');
  let customName = $state('');

  const filteredTags = $derived(tagFilter.trim()
    ? tags.filter(t => t.toLowerCase().includes(tagFilter.trim().toLowerCase()))
    : tags
  );

  let manualSource = $state('');
  let manualName = $state('');
  let showManualModal = $state(false);
  let manualLoading = $state(false);
  let manualResult = $state<'ok' | 'err' | ''>('');
  let manualMsg = $state('');

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

  function openLoad() { showLoad = true; loadResult = ''; loadMsg = ''; loadLogs = []; loadJobId = ''; searchQuery = ''; searchResults = []; selectedRepo = ''; selectedTag = ''; tags = []; tagPage = 1; hasMoreTags = false; tagFilter = ''; customName = ''; }
  function closeLoad() { if (!loading) { showLoad = false; loadJobId = ''; } }

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

  async function pickImage(repo: string) {
    selectedRepo = repo;
    selectedTag = 'latest';
    customName = repo;
    searchQuery = '';
    searchResults = [];
    tags = [];
    tagPage = 1;
    hasMoreTags = false;
    tagFilter = '';
    loadingTags = true;
    try {
      const res = await fetch(`/api/tags?repo=${encodeURIComponent(repo)}&page=1`);
      const data = await res.json();
      tags = data.tags || [];
      hasMoreTags = data.has_next;
    } catch { tags = []; }
    loadingTags = false;
  }

  async function loadMoreTags() {
    if (!hasMoreTags || loadingTags) return;
    loadingTags = true;
    const nextPage = tagPage + 1;
    try {
      const res = await fetch(`/api/tags?repo=${encodeURIComponent(selectedRepo)}&page=${nextPage}`);
      const data = await res.json();
      tags = [...tags, ...(data.tags || [])];
      hasMoreTags = data.has_next;
      tagPage = nextPage;
    } catch {}
    loadingTags = false;
  }

  function onTagScroll(e: Event) {
    const el = e.target as HTMLElement;
    if (el.scrollTop + el.clientHeight >= el.scrollHeight - 10) {
      loadMoreTags();
    }
  }

  function startManual() {}
  function openManual() {
    showManualModal = true;
    manualSource = '';
    manualName = '';
    manualResult = '';
    manualMsg = '';
    loadLogs = [];
    loadJobId = '';
  }

  async function doManualLoad() {
    const source = manualSource.trim();
    if (!source) return;
    const name = manualName.trim() || source.replace(/[:/@]/g, '-');
    manualLoading = true; manualResult = ''; manualMsg = '';
    try {
      const res = await fetch('/api/image/load', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ source, name })
      });
      const data = await res.json();
      if (data.job_id) {
        loadJobId = data.job_id;
        pollManualProgress();
      } else {
        manualResult = 'err'; manualMsg = 'Failed to start job';
        manualLoading = false;
      }
    } catch (e: any) { manualResult = 'err'; manualMsg = e.message || 'Load failed'; manualLoading = false; }
  }

  async function pollManualProgress() {
    if (!loadJobId) return;
    try {
      const res = await fetch(`/api/image/load/progress?job=${loadJobId}`);
      const data = await res.json();
      loadLogs = [...(data.logs || [])];
      if (data.status === 'done') {
        manualResult = 'ok'; manualMsg = data.result; manualLoading = false;
        refresh();
      } else if (data.status === 'error') {
        manualResult = 'err'; manualMsg = data.result || 'Unknown error'; manualLoading = false;
      } else {
        setTimeout(pollManualProgress, 500);
      }
    } catch (e: any) {
      manualResult = 'err'; manualMsg = e.message; manualLoading = false;
    }
  }

  async function doLoad() {
    if (!selectedRepo) return;
    const source = selectedTag ? `${selectedRepo}:${selectedTag}` : selectedRepo;
    const name = customName.trim();
    if (!name) return;
    loading = true; loadResult = ''; loadMsg = ''; loadLogs = []; loadJobId = '';
    try {
      const res = await fetch('/api/image/load', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ source, name })
      });
      const data = await res.json();
      if (data.job_id) {
        loadJobId = data.job_id;
        pollLoadProgress();
      } else {
        loadResult = 'err'; loadMsg = 'Failed to start job';
        loading = false;
      }
    } catch (e: any) { loadResult = 'err'; loadMsg = e.message || 'Load failed'; loading = false; }
  }

  async function pollLoadProgress() {
    if (!loadJobId) return;
    try {
      const res = await fetch(`/api/image/load/progress?job=${loadJobId}`);
      const data = await res.json();
      loadLogs = [...(data.logs || [])];
      if (data.status === 'done') {
        loadResult = 'ok'; loadMsg = data.result; loading = false;
        refresh();
      } else if (data.status === 'error') {
        loadResult = 'err'; loadMsg = data.result || 'Unknown error'; loading = false;
      } else {
        setTimeout(pollLoadProgress, 500);
      }
    } catch (e: any) {
      loadResult = 'err'; loadMsg = e.message; loading = false;
    }
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

{#if showManualModal}
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <div class="overlay" onclick={() => showManualModal = false} onkeydown={(e) => { if (e.key === 'Escape') showManualModal = false; }}>
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div class="modal" onclick={(e) => e.stopPropagation()} onkeydown={() => {}}>
      <div class="modal-head">
        <span>Manual <em>Entry</em></span>
        <button class="ghost" onclick={() => showManualModal = false} disabled={manualLoading}>Close</button>
      </div>

      {#if manualResult === 'ok'}
        <div class="msg ok">{manualMsg}</div>
        <button class="ghost" style="margin-top:12px" onclick={() => showManualModal = false}>Close</button>
      {:else if manualResult === 'err'}
        <div class="msg err">{manualMsg}</div>
        <button class="ghost" style="margin-top:12px" onclick={() => { manualResult = ''; manualMsg = ''; }}>Try again</button>
      {:else if manualLoading}
        <div class="field">
          <label>Source</label>
          <div class="picked-repo">{manualSource}</div>
        </div>
        <div class="log-console">
          {#each loadLogs as line}
            <div class="log-line">{line}</div>
          {/each}
        </div>
        <div class="dim" style="font-size:10px;text-align:center;margin-bottom:8px">Do not close or refresh this page</div>
      {:else}
        <div class="field">
          <label for="msrc">Source</label>
          <input id="msrc" bind:value={manualSource} placeholder="nginx:alpine  or  https://github.com/user/repo.git" disabled={manualLoading} />
          <div class="dim" style="font-size:10px;margin-top:4px">Docker Hub image:tag, or Git repository URL</div>
        </div>
        <div class="field">
          <label for="mname">Name</label>
          <input id="mname" bind:value={manualName} placeholder={manualSource.replace(/[:/@]/g, '-') || 'my-image'} disabled={manualLoading} />
          <div class="dim" style="font-size:10px;margin-top:4px">Required. Identifies the image in KDCT.</div>
        </div>
        <button class="primary" style="width:100%" onclick={doManualLoad} disabled={manualLoading || !manualSource.trim()}>
          {manualLoading ? 'Loading...' : 'Load Image'}
        </button>
      {/if}
    </div>
  </div>
{/if}

{#if images.length === 0}
  <div class="empty">No images loaded. Click <em>+ Load Image</em> above to load a Docker image or Git repository.</div>
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
        <button class="primary" onclick={doDeploy} disabled={deploying || !deployNid}>
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
        <button class="ghost" style="margin-top:12px" onclick={() => showLoad = false}>Close</button>
      {:else if loadResult === 'err'}
        <div class="msg err">{loadMsg}</div>
        <button class="ghost" style="margin-top:12px" onclick={() => { loadResult = ''; loadMsg = ''; }}>Try again</button>
        {:else}
        {#if !selectedRepo}
          <div class="section-head" style="margin-bottom:10px"><h2>Docker Hub</h2></div>
          <div class="field">
            <input bind:value={searchQuery} oninput={handleSearchInput} placeholder="Search nginx, redis, postgres..." disabled={loading} />
          </div>
          {#if searching}
            <div class="dim" style="font-size:11px;padding:4px 0 8px">Searching...</div>
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

          <button class="ghost" style="width:100%;margin-top:12px" onclick={() => { showLoad = false; openManual(); }}>Manual entry</button>
        {:else if selectedRepo}
          {#if loading}
            <div class="field">
              <label>Image</label>
              <div class="picked-repo">{selectedRepo}{selectedTag ? `:${selectedTag}` : ''}</div>
            </div>
            <div class="log-console">
              {#each loadLogs as line}
                <div class="log-line">{line}</div>
              {/each}
            </div>
            <div class="dim" style="font-size:10px;text-align:center;margin-bottom:8px">Do not close or refresh this page</div>
          {:else}
          <div class="field">
            <label>Image</label>
            <div class="picked-repo">{selectedRepo}</div>
          </div>
          <div class="field">
            <label for="tag">Tag</label>
            {#if loadingTags && tags.length === 0}
              <div class="dim" style="font-size:11px">Loading tags...</div>
            {:else}
              <input style="margin-bottom:6px" bind:value={tagFilter} placeholder="Filter tags..." disabled={loading} />
              <div class="tag-list" onscroll={onTagScroll}>
                {#each filteredTags as t}
                  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
                  <div
                    class="tag-item"
                    class:selected={selectedTag === t}
                    onclick={() => selectedTag = selectedTag === t ? '' : t}
                    onkeydown={(e) => { if (e.key === 'Enter') selectedTag = selectedTag === t ? '' : t; }}
                    role="option"
                    tabindex="0"
                  >
                    {t}
                  </div>
                {/each}
                {#if filteredTags.length === 0 && tagFilter.trim()}
                  <div class="dim" style="font-size:10px;padding:6px 10px">No matching tags</div>
                {/if}
                {#if loadingTags}
                  <div class="dim" style="font-size:10px;padding:6px 10px">Loading more...</div>
                {:else if hasMoreTags}
                  <div class="dim" style="font-size:10px;padding:6px 10px">Scroll for more...</div>
                {/if}
              </div>
            {/if}
          </div>
          <div class="field">
            <label for="cname">Name</label>
            <input id="cname" bind:value={customName} placeholder="My custom name" disabled={loading} />
            <div class="dim" style="font-size:10px;margin-top:4px">Required. This name identifies the image in KDCT.</div>
          </div>
          <div style="margin-bottom:12px">
            <button class="ghost" onclick={() => selectedRepo = ''}>Change image</button>
          </div>
          <button class="primary" onclick={doLoad} disabled={loading || !customName.trim()}>
            {loading ? 'Loading...' : 'Load Image'}
          </button>
          {/if}
        {/if}
      {/if}
    </div>
  </div>
{/if}
