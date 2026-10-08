import type { ModelColors } from '../types';

// Print-size presets: physical size of the printed model, in cm per side.
// Selecting one drives both the fixed selection square on the map and the
// `print_size` request field (cm × 10 = mm).
export interface SizePreset {
  cm: number;
  hint: string;
}

export const SIZE_PRESETS: SizePreset[] = [
  { cm: 26, hint: 'Large plate' },
  { cm: 20, hint: 'Standard' },
  { cm: 14, hint: 'Desk size' },
  { cm: 50, hint: 'Multi-plate' },
];

export const DEFAULT_PRINT_SIZE_MM = 260;

// Colors rendered in the 3D preview. Roads / green areas are wired UI-side
// and will take effect once the backend tags those mesh regions.
export const DEFAULT_COLORS: ModelColors = {
  terrain: '#6aaa55', // green
  buildings: '#f2efe9', // chalk white
  greenAreas: '#4c9a43', // green
  roads: '#f2efe9', // chalk white
  water: '#8ec9e6', // light blue
};

export interface ColorRow {
  key: keyof ModelColors;
  label: string;
  icon: string;
  pending?: boolean; // not yet rendered by the backend mesh
}

export const COLOR_ROWS: ColorRow[] = [
  { key: 'terrain', label: 'Terrain', icon: '⛰️' },
  { key: 'buildings', label: 'Buildings', icon: '🏢' },
  { key: 'greenAreas', label: 'Green areas', icon: '🌳', pending: true },
  { key: 'roads', label: 'Roads', icon: '🛣️', pending: true },
  { key: 'water', label: 'Sea / River / Lake', icon: '💧' },
];
