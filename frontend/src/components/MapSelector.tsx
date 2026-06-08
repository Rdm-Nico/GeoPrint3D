import { useEffect, useRef, useState } from 'react';
import { MapContainer, TileLayer, Rectangle, useMap, useMapEvents } from 'react-leaflet';
import L, { type LatLngBounds } from 'leaflet';
import 'leaflet/dist/leaflet.css';
import type { BoundingBox } from '../types';

interface MapSelectorProps {
  onBoundsChange: (bbox: BoundingBox | null) => void;
}

// ── Nominatim types ────────────────────────────────────────────────────────────
interface NominatimResult {
  lat: string;
  lon: string;
  display_name: string;
  boundingbox: [string, string, string, string]; // [south, north, west, east]
}

// ── Search bar ─────────────────────────────────────────────────────────────────
// Live search: fires a Nominatim request 350 ms after the user stops typing.
// Shows the top 3 results in a dropdown; clicking one flies the map there.
function SearchControl() {
  const map = useMap();
  const [query, setQuery] = useState('');
  const [results, setResults] = useState<NominatimResult[]>([]);
  const [loading, setLoading] = useState(false);
  const [open, setOpen] = useState(false);
  const wrapperRef = useRef<HTMLDivElement>(null);

  // Debounced live search — fires 350 ms after typing stops
  useEffect(() => {
    const q = query.trim();
    if (!q) {
      setResults([]);
      setOpen(false);
      return;
    }

    setLoading(true);
    const timer = setTimeout(async () => {
      try {
        const res = await fetch(
          `https://nominatim.openstreetmap.org/search?q=${encodeURIComponent(q)}&format=json&limit=3`,
          { headers: { 'Accept-Language': 'en' } }
        );
        const data: NominatimResult[] = await res.json();
        setResults(data);
        setOpen(data.length > 0);
      } catch {
        // silently ignore network errors
      } finally {
        setLoading(false);
      }
    }, 350);

    return () => {
      clearTimeout(timer);
      setLoading(false);
    };
  }, [query]);

  function select(r: NominatimResult) {
    const [south, north, west, east] = r.boundingbox.map(Number);
    map.fitBounds([[south, west], [north, east]], { maxZoom: 16 });
    setQuery(r.display_name.split(',')[0]);
    setOpen(false);
  }

  // Close dropdown when clicking outside
  useEffect(() => {
    function handleClick(e: MouseEvent) {
      if (wrapperRef.current && !wrapperRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    }
    document.addEventListener('mousedown', handleClick);
    return () => document.removeEventListener('mousedown', handleClick);
  }, []);

  // Prevent Leaflet from swallowing keyboard/mouse/scroll events on the widget
  useEffect(() => {
    if (!wrapperRef.current) return;
    L.DomEvent.disableClickPropagation(wrapperRef.current);
    L.DomEvent.disableScrollPropagation(wrapperRef.current);
  }, []);

  return (
    <div
      ref={wrapperRef}
      className="absolute top-4 right-4 z-[1000] w-72"
      onMouseDown={e => e.stopPropagation()}
    >
      {/* Input */}
      <div className="flex items-center bg-white border border-gray-300 rounded-lg shadow-lg px-3 py-2 gap-2">
        <span className="text-gray-400 text-sm select-none">🔍</span>
        <input
          type="text"
          value={query}
          onChange={e => setQuery(e.target.value)}
          onKeyDown={e => { if (e.key === 'Escape') { setOpen(false); setQuery(''); } }}
          placeholder="Search places…"
          className="flex-1 text-sm bg-transparent focus:outline-none text-gray-800 placeholder-gray-400"
        />
        {loading && (
          <span className="text-gray-400 text-xs animate-pulse select-none">…</span>
        )}
        {query && !loading && (
          <button
            onClick={() => { setQuery(''); setResults([]); setOpen(false); }}
            className="text-gray-400 hover:text-gray-600 text-sm leading-none"
            title="Clear"
          >
            ✕
          </button>
        )}
      </div>

      {/* Dropdown results */}
      {open && (
        <ul className="mt-1 bg-white border border-gray-200 rounded-lg shadow-xl text-sm overflow-hidden">
          {results.map((r, i) => (
            <li key={i} className="border-b border-gray-100 last:border-0">
              <button
                onClick={() => select(r)}
                className="w-full text-left px-3 py-2.5 hover:bg-blue-50 text-gray-800 flex flex-col gap-0.5"
              >
                <span className="font-medium truncate">{r.display_name.split(',')[0]}</span>
                <span className="text-xs text-gray-400 truncate">
                  {r.display_name.split(',').slice(1).join(',').trim()}
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

// ── Bounding-box draw interaction ──────────────────────────────────────────────
function BoundingBoxSelector({
  onBoundsChange,
}: {
  onBoundsChange: (bbox: BoundingBox | null) => void;
}) {
  const [bounds, setBounds] = useState<LatLngBounds | null>(null);
  const [startPoint, setStartPoint] = useState<{ lat: number; lng: number } | null>(null);
  const [isSelecting, setIsSelecting] = useState(false);
  const [modifierPressed, setModifierPressed] = useState(false);

  const map = useMapEvents({
    mousedown: (e) => {
      const originalEvent = e.originalEvent as MouseEvent;
      if (originalEvent.metaKey || originalEvent.ctrlKey) {
        e.originalEvent.preventDefault();
        map.dragging.disable();
        setIsSelecting(true);
        setStartPoint({ lat: e.latlng.lat, lng: e.latlng.lng });
        setBounds(null);
        onBoundsChange(null);
      }
    },
    mousemove: (e) => {
      if (isSelecting && startPoint) {
        const boundsObj = L.latLngBounds(
          [startPoint.lat, startPoint.lng],
          [e.latlng.lat, e.latlng.lng]
        );
        setBounds(boundsObj);
      }
    },
    mouseup: (e) => {
      if (isSelecting && startPoint) {
        const finalBounds = L.latLngBounds(
          [startPoint.lat, startPoint.lng],
          [e.latlng.lat, e.latlng.lng]
        );

        setBounds(finalBounds);
        onBoundsChange({
          min_lat: finalBounds.getSouth(),
          max_lat: finalBounds.getNorth(),
          min_lon: finalBounds.getWest(),
          max_lon: finalBounds.getEast(),
        });

        setStartPoint(null);
        setIsSelecting(false);
        map.dragging.enable();
      }
    },
  });

  useEffect(() => {
    const down = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey) setModifierPressed(true);
    };
    const up = (e: KeyboardEvent) => {
      if (!e.metaKey && !e.ctrlKey) {
        setModifierPressed(false);
        if (!isSelecting) map.dragging.enable();
      }
    };
    window.addEventListener('keydown', down);
    window.addEventListener('keyup', up);
    return () => {
      window.removeEventListener('keydown', down);
      window.removeEventListener('keyup', up);
    };
  }, [map, isSelecting]);

  useEffect(() => {
    map.getContainer().style.cursor = modifierPressed ? 'crosshair' : 'grab';
  }, [modifierPressed, map]);

  const area = bounds ? calculateArea(bounds) : 0;
  const rectColor = area > 20 ? '#ef4444' : area > 10 ? '#f59e0b' : '#3b82f6';

  return bounds ? (
    <Rectangle
      bounds={bounds}
      pathOptions={{ color: rectColor, weight: 2, fillColor: rectColor, fillOpacity: 0.2 }}
    />
  ) : null;
}

function calculateArea(bounds: LatLngBounds): number {
  const latKm = (bounds.getNorth() - bounds.getSouth()) * 111.0;
  const lonKm =
    (bounds.getEast() - bounds.getWest()) *
    111.0 *
    Math.cos((bounds.getSouth() * Math.PI) / 180);
  return Math.abs(latKm * lonKm);
}

// ── Public export ──────────────────────────────────────────────────────────────
export default function MapSelector({ onBoundsChange }: MapSelectorProps) {
  const [mapKey, setMapKey] = useState(0);

  const resetMap = () => {
    setMapKey(prev => prev + 1);
    onBoundsChange(null);
  };

  return (
    <div className="relative w-full h-full">
      <MapContainer
        key={mapKey}
        center={[40.7128, -74.006]}
        zoom={13}
        className="w-full h-full"
        style={{ height: '100%', width: '100%' }}
      >
        <TileLayer
          attribution='&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors'
          url="https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png"
        />
        <BoundingBoxSelector onBoundsChange={onBoundsChange} />
        <SearchControl />
      </MapContainer>

      {/* Instructions card */}
      <div className="absolute top-4 left-4 bg-white p-4 rounded-lg shadow-lg z-[1000] max-w-xs">
        <h3 className="font-bold text-sm mb-2">How to Select Area:</h3>
        <ol className="text-xs space-y-1 text-gray-700">
          <li>
            1. Hold{' '}
            <kbd className="px-1 py-0.5 bg-gray-200 rounded text-xs font-mono">⌘ Cmd</kbd>{' '}
            (Mac) or{' '}
            <kbd className="px-1 py-0.5 bg-gray-200 rounded text-xs font-mono">Ctrl</kbd>{' '}
            (Windows)
          </li>
          <li>2. Click and drag to define bounding box</li>
          <li>3. Release to confirm (max 20 km²)</li>
        </ol>
        <div className="mt-2 p-2 bg-blue-50 rounded text-xs text-blue-800">
          💡 Without modifier key: pan/zoom map normally
        </div>
        <button
          onClick={resetMap}
          className="mt-3 w-full px-3 py-1 bg-blue-500 text-white text-xs rounded hover:bg-blue-600"
        >
          Reset Selection
        </button>
      </div>
    </div>
  );
}
