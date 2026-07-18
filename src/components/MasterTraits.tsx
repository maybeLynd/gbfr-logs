import { Accordion, Badge, Box, Group, Paper, SimpleGrid, Stack, Tabs, Text } from "@mantine/core";
import { useMemo } from "react";
import { useTranslation } from "react-i18next";

import MasterTraits, { MasterTraitCategory, MasterTraitCharacter, MasterTraitNode } from "@/assets/master-traits";
import { PlayerData } from "@/types";

const CATEGORY_LABELS: Record<MasterTraitCategory, string> = {
  SB_DEF: "Insight",
  SB_ATK: "Essence",
  SB_LIMIT: "Crux",
};

const CATEGORY_COLORS: Record<MasterTraitCategory, string> = {
  SB_DEF: "grape",
  SB_ATK: "blue",
  SB_LIMIT: "orange",
};

const RANK_REQUIREMENTS: Record<number, number> = {
  1: 3,
  2: 6,
  3: 6,
};

const playerLabel = (player: PlayerData, showDisplayNames: boolean, translate: (key: string) => string) => {
  const character =
    typeof player.characterType === "string" ? translate(`characters:${player.characterType}`) : "Unknown";
  return showDisplayNames && player.displayName ? `${player.displayName} (${character})` : character;
};

const activeNodesForCategory = (player: PlayerData, character: MasterTraitCharacter, category: MasterTraitCategory) =>
  (player.masterTraits || [])
    .map((id) => character.nodes[String(id)])
    .filter((node): node is MasterTraitNode => node?.category === category);

const MasterTraitBuild = ({ player }: { player: PlayerData }) => {
  const characterType = typeof player.characterType === "string" ? player.characterType : "";
  const masterTraitCharacterType = characterType === "Pl0100" ? "Pl0000" : characterType;
  const character = MasterTraits.characters[masterTraitCharacterType];
  const selected = useMemo(() => new Set(player.masterTraits || []), [player.masterTraits]);

  if (!character) {
    return (
      <Text size="sm" c="dimmed" mt="md">
        Master Traits are unavailable for this character.
      </Text>
    );
  }

  return (
    <Stack mt="md" gap="md">
      <SimpleGrid cols={{ base: 1, md: 3 }}>
        {MasterTraits.categories.map((category) => {
          const nodes = activeNodesForCategory(player, character, category);
          const style = character.styles[category];
          const pips = [1, 2, 3].map(
            (rank) => nodes.filter((node) => node.rank === rank).length >= RANK_REQUIREMENTS[rank]
          );
          return (
            <Paper key={category} withBorder p="md">
              <Text size="xs" c="dimmed">
                {CATEGORY_LABELS[category]}
              </Text>
              <Text fw={700}>{style?.name || CATEGORY_LABELS[category]}</Text>
              <Text size="xl" c={CATEGORY_COLORS[category]} aria-label={`${pips.filter(Boolean).length} active ranks`}>
                {pips.map((active) => (active ? "◆" : "◇")).join(" ")}
              </Text>
            </Paper>
          );
        })}
      </SimpleGrid>

      <Accordion multiple defaultValue={MasterTraits.categories} variant="separated">
        {MasterTraits.categories.map((category) => {
          const style = character.styles[category];
          const activeNodes = activeNodesForCategory(player, character, category);
          const allNodes = Object.values(character.nodes).filter((node) => node.category === category);

          return (
            <Accordion.Item key={category} value={category}>
              <Accordion.Control>
                <Group justify="space-between" pr="md">
                  <Text fw={700}>{style?.name || CATEGORY_LABELS[category]}</Text>
                  <Badge color={CATEGORY_COLORS[category]}>{activeNodes.length} selected</Badge>
                </Group>
              </Accordion.Control>
              <Accordion.Panel>
                <Stack gap="lg">
                  {[1, 2, 3, 4].map((rank) => {
                    const nodes = allNodes
                      .filter((node) => node.rank === rank)
                      .sort((left, right) => left.nodeIndex - right.nodeIndex);
                    const selectedCount = nodes.filter((node) => selected.has(node.id)).length;
                    const perk = style?.perks.find((candidate) => candidate.rank === rank);
                    if (!perk && nodes.length === 0) return null;
                    const required = RANK_REQUIREMENTS[rank];
                    const perkActive = perk ? selectedCount >= required : false;

                    return (
                      <Box key={rank}>
                        <Text size="sm" fw={700} mb="xs">
                          {rank === 4 ? "EX Rank" : `Style Rank ${rank}`}
                        </Text>
                        {perk && (
                          <Paper withBorder p="sm" mb="xs" bg={perkActive ? "var(--mantine-color-dark-6)" : undefined}>
                            <Text size="xs" fw={700} c={perkActive ? CATEGORY_COLORS[category] : "dimmed"}>
                              {perkActive ? "◆ Active" : "◇ Locked"} · {selectedCount}/{nodes.length} selected ·{" "}
                              {required} required
                            </Text>
                            <Text size="xs" c={perkActive ? undefined : "dimmed"} style={{ whiteSpace: "pre-line" }}>
                              {perk.description}
                            </Text>
                          </Paper>
                        )}
                        <SimpleGrid cols={{ base: 1, lg: 2 }}>
                          {nodes.map((node) => {
                            const isActive = selected.has(node.id);
                            return (
                              <Paper key={node.id} withBorder p="sm" opacity={isActive ? 1 : 0.45}>
                                <Text size="xs" fw={700} c={isActive ? CATEGORY_COLORS[category] : "dimmed"}>
                                  {isActive ? "◆ Selected" : "◇ Not selected"}
                                </Text>
                                <Text size="xs" style={{ whiteSpace: "pre-line" }}>
                                  {node.description}
                                </Text>
                              </Paper>
                            );
                          })}
                        </SimpleGrid>
                      </Box>
                    );
                  })}
                </Stack>
              </Accordion.Panel>
            </Accordion.Item>
          );
        })}
      </Accordion>
    </Stack>
  );
};

export const MasterTraitsPanel = ({
  players,
  showDisplayNames,
}: {
  players: PlayerData[];
  showDisplayNames: boolean;
}) => {
  const { t } = useTranslation();
  if (players.length === 0) return null;

  const defaultPlayer = String(
    players.find((player) => player.masterTraits?.length > 0)?.actorIndex ?? players[0].actorIndex
  );
  return (
    <Tabs defaultValue={defaultPlayer} mt="md" variant="pills">
      <Tabs.List>
        {players.map((player) => (
          <Tabs.Tab key={player.actorIndex} value={String(player.actorIndex)}>
            {playerLabel(player, showDisplayNames, t)}
          </Tabs.Tab>
        ))}
      </Tabs.List>
      {players.map((player) => (
        <Tabs.Panel key={player.actorIndex} value={String(player.actorIndex)}>
          <MasterTraitBuild player={player} />
        </Tabs.Panel>
      ))}
    </Tabs>
  );
};
