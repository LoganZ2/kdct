const BASE = '/api';

async function fetchJson<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${BASE}${url}`, init);
  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || `${res.status}`);
  }
  return res.json();
}

export function getOverview() { return fetchJson<any>('/overview'); }
export function getNodes() { return fetchJson<any[]>('/nodes'); }
export function getImages() { return fetchJson<any[]>('/images'); }
export function getImage(name: string) { return fetchJson<any>(`/images/${encodeURIComponent(name)}`); }
export function getDeployments() { return fetchJson<any[]>('/deployments'); }

export async function deployImage(imageName: string, nodeId: number): Promise<string> {
  const res = await fetch(`${BASE}/deploy`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ image: imageName, node_id: nodeId })
  });
  const text = await res.text();
  if (!res.ok) throw new Error(text);
  return text;
}

export async function stopImage(imageName: string, nodeId: number): Promise<string> {
  const res = await fetch(`${BASE}/stop`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ image: imageName, node_id: nodeId })
  });
  const text = await res.text();
  if (!res.ok) throw new Error(text);
  return text;
}
