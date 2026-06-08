import { Suspense, useRef, useMemo } from 'react';
import { Canvas, useFrame } from '@react-three/fiber';
import { OrbitControls, PerspectiveCamera, Grid } from '@react-three/drei';
import * as THREE from 'three';
import type { Mesh } from 'three';
import type { MeshData, MeshStats } from '../types';

interface Preview3DProps {
  meshData?: MeshData;
  stats?: MeshStats;
}

// Pre-compute the shared transform (centre + normalise to ~5 units) from all vertices.
// Both sub-meshes must use the same transform to stay aligned.
function computeTransform(vertices: [number, number, number][]) {
  let minX = Infinity, maxX = -Infinity;
  let minY = Infinity, maxY = -Infinity;
  let minZ = Infinity, maxZ = -Infinity;

  for (const [x, y, z] of vertices) {
    if (x < minX) minX = x; if (x > maxX) maxX = x;
    if (y < minY) minY = y; if (y > maxY) maxY = y;
    if (z < minZ) minZ = z; if (z > maxZ) maxZ = z;
  }

  const cx = (minX + maxX) / 2;
  const cy = (minY + maxY) / 2;
  const cz = (minZ + maxZ) / 2;
  const maxDim = Math.max(maxX - minX, maxY - minY, maxZ - minZ) || 1;
  const scale = 5 / maxDim;

  return { cx, cy, cz, scale };
}

// Build a Float32Array of transformed positions (shared between terrain and building geometries).
function buildPositions(
  vertices: [number, number, number][],
  cx: number, cy: number, cz: number, scale: number
): Float32Array {
  const arr = new Float32Array(vertices.length * 3);
  for (let i = 0; i < vertices.length; i++) {
    arr[i * 3]     = (vertices[i][0] - cx) * scale;
    arr[i * 3 + 1] = (vertices[i][1] - cy) * scale;
    arr[i * 3 + 2] = (vertices[i][2] - cz) * scale;
  }
  return arr;
}

// Build a Uint32Array of indices from a slice of the triangles array.
function buildIndices(
  triangles: [number, number, number][],
  start: number,
  end: number
): Uint32Array {
  const count = end - start;
  const arr = new Uint32Array(count * 3);
  for (let i = 0; i < count; i++) {
    const tri = triangles[start + i];
    arr[i * 3]     = tri[0];
    arr[i * 3 + 1] = tri[1];
    arr[i * 3 + 2] = tri[2];
  }
  return arr;
}

// Renders the real terrain mesh split into two coloured layers:
//   - Terrain surface → green
//   - Building extrusions → concrete grey
function TerrainMesh({ meshData }: { meshData: MeshData }) {
  const { terrainGeo, buildingGeo } = useMemo(() => {
    const { cx, cy, cz, scale } = computeTransform(meshData.vertices);
    const positions = buildPositions(meshData.vertices, cx, cy, cz, scale);

    const terrainTc = meshData.terrain_triangle_count;
    const buildingTc = meshData.building_triangle_count;

    // Terrain geometry
    const tGeo = new THREE.BufferGeometry();
    tGeo.setAttribute('position', new THREE.BufferAttribute(positions.slice(), 3));
    tGeo.setIndex(new THREE.BufferAttribute(buildIndices(meshData.triangles, 0, terrainTc), 1));
    tGeo.computeVertexNormals();

    // Building geometry (may be empty)
    let bGeo: THREE.BufferGeometry | null = null;
    if (buildingTc > 0) {
      bGeo = new THREE.BufferGeometry();
      bGeo.setAttribute('position', new THREE.BufferAttribute(positions.slice(), 3));
      bGeo.setIndex(new THREE.BufferAttribute(
        buildIndices(meshData.triangles, terrainTc, terrainTc + buildingTc), 1
      ));
      bGeo.computeVertexNormals();
    }

    return { terrainGeo: tGeo, buildingGeo: bGeo };
  }, [meshData]);

  return (
    // rotate so Z-up (backend) maps to Y-up (Three.js)
    <group rotation={[-Math.PI / 2, 0, 0]}>
      <mesh geometry={terrainGeo}>
        <meshStandardMaterial
          color="#6aaa55"
          roughness={0.8}
          metalness={0.0}
          flatShading={true}
          side={THREE.DoubleSide}
        />
      </mesh>
      {buildingGeo && (
        <mesh geometry={buildingGeo}>
          <meshStandardMaterial
            color="#c8b89a"
            roughness={0.6}
            metalness={0.1}
            flatShading={true}
            side={THREE.DoubleSide}
          />
        </mesh>
      )}
    </group>
  );
}

// Placeholder shown before any mesh is generated
function TerrainPlaceholder() {
  const meshRef = useRef<Mesh>(null);

  useFrame((state) => {
    if (meshRef.current) {
      meshRef.current.rotation.z = Math.sin(state.clock.elapsedTime * 0.5) * 0.1;
    }
  });

  return (
    <mesh ref={meshRef} rotation={[-Math.PI / 2, 0, 0]}>
      <planeGeometry args={[5, 5, 20, 20]} />
      <meshStandardMaterial color="#4ade80" roughness={0.8} metalness={0.2} />
    </mesh>
  );
}

export default function Preview3D({ meshData, stats }: Preview3DProps) {
  return (
    <div className="w-full h-full bg-gradient-to-br from-gray-900 to-gray-800 rounded-lg overflow-hidden relative">
      <Canvas>
        <PerspectiveCamera makeDefault position={[8, 8, 8]} fov={50} />
        <OrbitControls
          enablePan={true}
          enableZoom={true}
          enableRotate={true}
          minDistance={3}
          maxDistance={20}
        />

        {/* Lighting */}
        <ambientLight intensity={0.4} />
        <directionalLight position={[10, 10, 5]} intensity={1} castShadow />
        <directionalLight position={[-10, -10, -5]} intensity={0.3} />
        <hemisphereLight intensity={0.3} />

        <Suspense fallback={null}>
          {meshData && meshData.vertices.length > 0 ? (
            <TerrainMesh meshData={meshData} />
          ) : (
            <TerrainPlaceholder />
          )}
        </Suspense>

        <Grid
          args={[10, 10]}
          cellSize={0.5}
          cellThickness={0.5}
          cellColor="#6b7280"
          sectionSize={2}
          sectionThickness={1}
          sectionColor="#374151"
          fadeDistance={25}
          fadeStrength={1}
          followCamera={false}
          position={[0, -0.01, 0]}
        />
      </Canvas>

      {/* Status */}
      <div className="absolute top-4 left-4 bg-black bg-opacity-70 text-white px-3 py-1 rounded text-sm">
        {meshData && meshData.vertices.length > 0 ? (
          <span className="text-green-400">3D Model Loaded</span>
        ) : (
          <span className="text-yellow-400">Select an area to generate</span>
        )}
      </div>

      {/* Stats */}
      {stats && (
        <div className="absolute bottom-4 left-4 bg-black bg-opacity-70 text-white p-3 rounded text-xs space-y-1">
          <div>Vertices: {stats.vertices.toLocaleString()}</div>
          <div>Triangles: {stats.triangles.toLocaleString()}</div>
          <div>Buildings: {stats.buildings_count.toLocaleString()}</div>
          <div>Elevation: {stats.min_elevation.toFixed(1)}m – {stats.max_elevation.toFixed(1)}m</div>
          <div>Area: {stats.area_km2.toFixed(4)} km²</div>
        </div>
      )}

      {/* Controls legend */}
      <div className="absolute top-4 right-4 bg-black bg-opacity-70 text-white p-3 rounded text-xs space-y-1">
        <div className="flex items-center gap-2">
          <span className="inline-block w-3 h-3 rounded-sm" style={{ background: '#6aaa55' }} />
          <span>Terrain</span>
        </div>
        <div className="flex items-center gap-2">
          <span className="inline-block w-3 h-3 rounded-sm" style={{ background: '#c8b89a' }} />
          <span>Buildings</span>
        </div>
        <div className="mt-2 pt-2 border-t border-gray-600 space-y-1">
          <div>Left drag: Rotate</div>
          <div>Right drag: Pan</div>
          <div>Scroll: Zoom</div>
        </div>
      </div>
    </div>
  );
}
