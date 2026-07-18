import { readTextFile } from "@tauri-apps/api/fs";
import { resolveResource } from "@tauri-apps/api/path";

export type MasterTraitNode = {
  id: number;
  category: MasterTraitCategory;
  rank: number;
  points: number;
  nodeIndex: number;
  description: string;
};

export type MasterTraitCategory = "SB_DEF" | "SB_ATK" | "SB_LIMIT";

export type MasterTraitCharacter = {
  styles: Record<
    MasterTraitCategory,
    {
      name: string;
      perks: Array<{ rank: number; pointsRequired: number; description: string }>;
    }
  >;
  nodes: Record<string, MasterTraitNode>;
};

type MasterTraitsResource = {
  version: string;
  categories: MasterTraitCategory[];
  characters: Record<string, MasterTraitCharacter>;
};

const resourcePath = await resolveResource("assets/master-traits.json");
const MasterTraits = JSON.parse(await readTextFile(resourcePath)) as MasterTraitsResource;

export default MasterTraits;
