import axios from 'axios';
import type { GenerateRequest, GenerateResponse, MeshData } from '../types';

const API_BASE_URL = '/api';

/**
 * Convert mesh data to binary STL format
 */
function meshToSTL(meshData: MeshData): ArrayBuffer {
  const { vertices, triangles } = meshData;

  // STL binary format:
  // 80 bytes header
  // 4 bytes: number of triangles (uint32)
  // For each triangle:
  //   12 bytes: normal vector (3 x float32)
  //   36 bytes: 3 vertices (3 x 3 x float32)
  //   2 bytes: attribute byte count (uint16, usually 0)

  const numTriangles = triangles.length;
  const bufferSize = 80 + 4 + numTriangles * 50;
  const buffer = new ArrayBuffer(bufferSize);
  const view = new DataView(buffer);

  // Header (80 bytes) - can be anything
  const header = 'GeoPrint3D - 3D Terrain Model';
  for (let i = 0; i < 80; i++) {
    view.setUint8(i, i < header.length ? header.charCodeAt(i) : 0);
  }

  // Number of triangles
  view.setUint32(80, numTriangles, true);

  let offset = 84;

  for (const [i0, i1, i2] of triangles) {
    const v0 = vertices[i0];
    const v1 = vertices[i1];
    const v2 = vertices[i2];

    // Calculate normal using cross product
    const ax = v1[0] - v0[0];
    const ay = v1[1] - v0[1];
    const az = v1[2] - v0[2];
    const bx = v2[0] - v0[0];
    const by = v2[1] - v0[1];
    const bz = v2[2] - v0[2];

    let nx = ay * bz - az * by;
    let ny = az * bx - ax * bz;
    let nz = ax * by - ay * bx;

    // Normalize
    const len = Math.sqrt(nx * nx + ny * ny + nz * nz);
    if (len > 0) {
      nx /= len;
      ny /= len;
      nz /= len;
    }

    // Write normal
    view.setFloat32(offset, nx, true); offset += 4;
    view.setFloat32(offset, ny, true); offset += 4;
    view.setFloat32(offset, nz, true); offset += 4;

    // Write vertices
    view.setFloat32(offset, v0[0], true); offset += 4;
    view.setFloat32(offset, v0[1], true); offset += 4;
    view.setFloat32(offset, v0[2], true); offset += 4;

    view.setFloat32(offset, v1[0], true); offset += 4;
    view.setFloat32(offset, v1[1], true); offset += 4;
    view.setFloat32(offset, v1[2], true); offset += 4;

    view.setFloat32(offset, v2[0], true); offset += 4;
    view.setFloat32(offset, v2[1], true); offset += 4;
    view.setFloat32(offset, v2[2], true); offset += 4;

    // Attribute byte count (0)
    view.setUint16(offset, 0, true); offset += 2;
  }

  return buffer;
}

export const api = {
  /**
   * Check backend health
   */
  healthCheck: async () => {
    const response = await axios.get(`${API_BASE_URL}/health`);
    return response.data;
  },

  /**
   * Generate 3D terrain model from bounding box
   */
  generateTerrain: async (request: GenerateRequest): Promise<GenerateResponse> => {
    const response = await axios.post<GenerateResponse>(
      `${API_BASE_URL}/generate`,
      request,
      {
        timeout: 120000, // 2 minutes timeout for heavy processing
      }
    );
    return response.data;
  },

  /**
   * Export mesh data to STL file and trigger download
   */
  downloadSTL: (meshData: MeshData, filename: string = 'terrain.stl') => {
    const stlBuffer = meshToSTL(meshData);
    const blob = new Blob([stlBuffer], { type: 'application/octet-stream' });
    const url = URL.createObjectURL(blob);

    const link = document.createElement('a');
    link.href = url;
    link.download = filename;
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);

    URL.revokeObjectURL(url);
  },
};
