export interface ClientNode {
  id: number;
  hostname: string;
  os: string;
  arch: string;
  docker_version: string;
  port_range_start: number;
  port_range_end: number;
  cpu_cores: number;
  memory_mb: number;
  status: string;
  last_seen: number;
}

export interface ImageNode {
  id: number;
  name: string;
  source: string;
  source_type: string;
  status: string;
  created_at: number;
}

export interface ImageDetail extends ImageNode {
  ports: { id: number; image_node_id: number; port: number; protocol: string; route_path: string | null }[];
  envs: { key: string; value: string }[];
}

export interface RunningContainer {
  container_name: string;
  image: string;
  hostname: string;
  ports: number[];
  status: string;
}

export interface Overview {
  node_count: number;
  online_count: number;
  image_count: number;
  configured_count: number;
  deployment_count: number;
  container_count: number;
}
