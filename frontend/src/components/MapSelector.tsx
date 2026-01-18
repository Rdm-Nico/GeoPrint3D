import { useEffect, useState } from 'react';
import { MapContainer, TileLayer, Rectangle, useMapEvents } from 'react-leaflet';
import type { LatLngBounds } from 'leaflet';
import 'leaflet/dist/leaflet.css';
import type { BoundingBox } from '../types';

interface MapSelectorProps {
  onBoundsChange: (bbox: BoundingBox | null) => void;
}

function BoundingBoxSelector({ onBoundsChange }: { onBoundsChange: (bbox: BoundingBox | null) => void }) {
  const [bounds, setBounds] = useState<LatLngBounds | null>(null);
  const [startPoint, setStartPoint] = useState<{ lat: number; lng: number } | null>(null);
  const [isSelecting, setIsSelecting] = useState(false);
  const [modifierPressed, setModifierPressed] = useState(false);

  // Listen for modifier key press/release globally
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey) {
        setModifierPressed(true);
      }
    };

    const handleKeyUp = (e: KeyboardEvent) => {
      if (!e.metaKey && !e.ctrlKey) {
        setModifierPressed(false);
      }
    };

    window.addEventListener('keydown', handleKeyDown);
    window.addEventListener('keyup', handleKeyUp);

    return () => {
      window.removeEventListener('keydown', handleKeyDown);
      window.removeEventListener('keyup', handleKeyUp);
    };
  }, []);

  const map = useMapEvents({
    mousedown: (e) => {
      // Only start selection if Command (Mac) or Ctrl (Windows/Linux) is pressed
      const originalEvent = e.originalEvent as MouseEvent;
      if (originalEvent.metaKey || originalEvent.ctrlKey) {
        e.originalEvent.preventDefault(); // Prevent default map behavior
        setIsSelecting(true);
        setStartPoint({ lat: e.latlng.lat, lng: e.latlng.lng });
        setBounds(null);
        onBoundsChange(null);
      }
    },
    mousemove: (e) => {
      if (isSelecting && startPoint) {
        const newBounds = [
          [startPoint.lat, startPoint.lng],
          [e.latlng.lat, e.latlng.lng],
        ] as [[number, number], [number, number]];

        const boundsObj = map.latLngBounds(newBounds);
        setBounds(boundsObj);
      }
    },
    mouseup: (e) => {
      if (isSelecting && startPoint) {
        const finalBounds = map.latLngBounds([
          [startPoint.lat, startPoint.lng],
          [e.latlng.lat, e.latlng.lng],
        ]);

        // Calculate area
        const area = calculateArea(finalBounds);

        if (area > 1.0) {
          alert(`Selected area (${area.toFixed(2)} km²) exceeds 1 km² limit. Please select a smaller area.`);
          setBounds(null);
          onBoundsChange(null);
        } else {
          setBounds(finalBounds);
          onBoundsChange({
            min_lat: finalBounds.getSouth(),
            max_lat: finalBounds.getNorth(),
            min_lon: finalBounds.getWest(),
            max_lon: finalBounds.getEast(),
          });
        }

        setStartPoint(null);
        setIsSelecting(false);
      }
    },
  });

  // Calculate approximate area in km²
  function calculateArea(bounds: LatLngBounds): number {
    const lat1 = bounds.getSouth();
    const lat2 = bounds.getNorth();
    const lon1 = bounds.getWest();
    const lon2 = bounds.getEast();

    const latKm = (lat2 - lat1) * 111.0;
    const lonKm = (lon2 - lon1) * 111.0 * Math.cos((lat1 * Math.PI) / 180);

    return Math.abs(latKm * lonKm);
  }

  // Apply cursor style to map container based on modifier key
  useEffect(() => {
    const container = map.getContainer();
    if (modifierPressed) {
      container.style.cursor = 'crosshair';
    } else {
      container.style.cursor = 'grab';
    }
  }, [modifierPressed, map]);

  return bounds ? (
    <Rectangle
      bounds={bounds}
      pathOptions={{
        color: '#3b82f6',
        weight: 2,
        fillColor: '#3b82f6',
        fillOpacity: 0.2,
      }}
    />
  ) : null;
}

export default function MapSelector({ onBoundsChange }: MapSelectorProps) {
  const [mapKey, setMapKey] = useState(0);

  // Reset map when needed
  const resetMap = () => {
    setMapKey(prev => prev + 1);
    onBoundsChange(null);
  };

  return (
    <div className="relative w-full h-full">
      <MapContainer
        key={mapKey}
        center={[40.7128, -74.0060]} // New York City default
        zoom={13}
        className="w-full h-full"
        style={{ height: '100%', width: '100%' }}
      >
        <TileLayer
          attribution='&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors'
          url="https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png"
        />
        <BoundingBoxSelector onBoundsChange={onBoundsChange} />
      </MapContainer>

      <div className="absolute top-4 left-4 bg-white p-4 rounded-lg shadow-lg z-[1000] max-w-xs">
        <h3 className="font-bold text-sm mb-2">How to Select Area:</h3>
        <ol className="text-xs space-y-1 text-gray-700">
          <li>1. Hold <kbd className="px-1 py-0.5 bg-gray-200 rounded text-xs font-mono">⌘ Cmd</kbd> (Mac) or <kbd className="px-1 py-0.5 bg-gray-200 rounded text-xs font-mono">Ctrl</kbd> (Windows)</li>
          <li>2. Click and drag to define bounding box</li>
          <li>3. Release to confirm (max 1 km²)</li>
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
