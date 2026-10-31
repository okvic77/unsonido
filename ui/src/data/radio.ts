/** Curated, publicly streamable radio suggestions for the Background tab.
 *
 * Picks with long-stable direct-stream URLs so a click-and-play years from now
 * still works: SomaFM's documented 128k MP3 mounts and Radio Paradise's
 * documented stream endpoints. Both are listener-supported independent radio.
 */

export interface Station {
  name: string;
  url: string;
  group: string;
}

export const STATIONS: Station[] = [
  // Chill
  { name: "Groove Salad", url: "https://ice1.somafm.com/groovesalad-128-mp3", group: "Chill" },
  { name: "Groove Salad Classic", url: "https://ice1.somafm.com/gsclassic-128-mp3", group: "Chill" },
  { name: "Lush", url: "https://ice1.somafm.com/lush-128-mp3", group: "Chill" },
  { name: "Fluid", url: "https://ice1.somafm.com/fluid-128-mp3", group: "Chill" },
  { name: "Deep Space One", url: "https://ice1.somafm.com/deepspaceone-128-mp3", group: "Chill" },

  // Ambient
  { name: "Drone Zone", url: "https://ice1.somafm.com/dronezone-128-mp3", group: "Ambient" },
  { name: "Space Station Soma", url: "https://ice1.somafm.com/spacestation-128-mp3", group: "Ambient" },

  // Beats
  { name: "The Trip", url: "https://ice1.somafm.com/thetrip-128-mp3", group: "Beats" },
  { name: "Beat Blender", url: "https://ice1.somafm.com/beatblender-128-mp3", group: "Beats" },
  { name: "Cliqhop", url: "https://ice1.somafm.com/cliqhop-128-mp3", group: "Beats" },
  { name: "Dub Step Beyond", url: "https://ice1.somafm.com/dubstep-128-mp3", group: "Beats" },
  { name: "DEF CON Radio", url: "https://ice1.somafm.com/defcon-128-mp3", group: "Beats" },

  // Lounge & retro
  { name: "Secret Agent", url: "https://ice1.somafm.com/secretagent-128-mp3", group: "Lounge & retro" },
  { name: "Illinois Street Lounge", url: "https://ice1.somafm.com/illstreet-128-mp3", group: "Lounge & retro" },
  { name: "Left Coast 70s", url: "https://ice1.somafm.com/seventies-128-mp3", group: "Lounge & retro" },

  // Rock & folk
  { name: "Indie Pop Rocks", url: "https://ice1.somafm.com/indiepop-128-mp3", group: "Rock & folk" },
  { name: "Boot Liquor", url: "https://ice1.somafm.com/bootliquor-128-mp3", group: "Rock & folk" },
  { name: "Digitalis", url: "https://ice1.somafm.com/digitalis-128-mp3", group: "Rock & folk" },

  // Eclectic
  { name: "Radio Paradise", url: "https://stream.radioparadise.com/mp3-128", group: "Eclectic" },
  { name: "Radio Paradise Mellow", url: "https://stream.radioparadise.com/mellow-128", group: "Eclectic" },
];

/** Groups in first-seen order. */
export function stationGroups(): string[] {
  return [...new Set(STATIONS.map((s) => s.group))];
}