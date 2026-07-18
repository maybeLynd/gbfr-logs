import { describe, expect, it } from "vitest";
import { getDamageOverTimeTranslationKeys, getSkillTranslationKeys, toHash, toHashString } from "./utils";

describe("utils", () => {
  it("toHash", () => {
    expect(toHash(1)).toBe("1");
    expect(toHash(255)).toBe("ff");
  });

  it("toHashString", () => {
    expect(toHashString(1)).toBe("00000001");
    expect(toHashString(255)).toBe("000000ff");
  });

  it("falls back from a game 2 skill variant to its ability slot", () => {
    expect(getSkillTranslationKeys("Pl2800", 1110)).toEqual(["skills.Pl2800.1110", "skills.Pl2800.1100"]);
  });

  it("does not merge unknown or legacy character action IDs", () => {
    expect(getSkillTranslationKeys("Pl2300", 1510)).toEqual(["skills.Pl2300.1510"]);
    expect(getSkillTranslationKeys({ Unknown: 123 }, 1101)).toEqual([]);
  });

  it("uses named DoTs while preserving character-specific DoT names", () => {
    expect(getDamageOverTimeTranslationKeys("Pl2300", "Pl2300", 0)).toEqual([
      "skills.Pl2300.damage-over-time-0",
      "skills.Pl2300.damage-over-time",
      "skills.default.damage-over-time-0",
      "skills.default.damage-over-time",
    ]);
    expect(getDamageOverTimeTranslationKeys("Pl2100", "Pl2100", 1)).toContain("skills.Pl2100.damage-over-time");
  });
});
