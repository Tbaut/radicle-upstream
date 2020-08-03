import { SvelteComponent } from "svelte";
import { Readable } from "svelte/store";

import * as history from "./history";

import Blank from "../Screen/Blank.svelte";
import Help from "../Screen/Help.svelte";

export interface View {
  readonly component: unknown;
  readonly props?: Props;
}

type Props = Record<string, 0 | string>;
export type ComponentMap<Key extends string, C extends typeof SvelteComponent> = Required<
  Record<Key, C>
>;

export interface Navigation<Key extends string> {
  readonly current: Readable<View>;
  back(): void;
  set(key: Key, props?: Props): void;
}

export const create = <K extends string, C extends typeof SvelteComponent>(
  componentMap: ComponentMap<K, C>,
  initial: K
): Navigation<K> => {
  const hist = history.create<View>({ component: componentMap[initial] });

  return {
    current: hist.current,
    back: (): void => {
      hist.pop();
    },
    set: (key: K, props?: Props): void => {
      hist.push({
        component: componentMap[key],
        props,
      });
    },
  };
};

export enum Screen {
  Blank = "Blank",
  Help = "Help",
}

export const screens: ComponentMap<Screen, typeof SvelteComponent> = {
  [Screen.Blank]: Blank,
  [Screen.Help]: Help,
};