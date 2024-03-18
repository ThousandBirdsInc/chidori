import { writable, derived } from 'svelte/store';
import { clamp, Rect, Vec2 } from '../lib/math';
import type { CallTreeNode, Frame, Profile, ProfileGroup } from '../lib/profile';
import { objectsHaveShallowEquality } from '../lib/utils';

export interface FlamechartViewState {
  hover: {
    node: CallTreeNode;
    event: MouseEvent;
  } | null;
  selectedNode: CallTreeNode | null;
  logicalSpaceViewportSize: Vec2;
  configSpaceViewportRect: Rect;
}

interface CallerCalleeState {
  selectedFrame: Frame;
  invertedCallerFlamegraph: FlamechartViewState;
  calleeFlamegraph: FlamechartViewState;
}

interface SandwichViewState {
  callerCallee: CallerCalleeState | null;
}

export interface ProfileState {
  profile: Profile;
  chronoViewState: FlamechartViewState;
  leftHeavyViewState: FlamechartViewState;
  sandwichViewState: SandwichViewState;
}

type ProfileGroupState = {
  name: string;
  indexToView: number;
  profiles: ProfileState[];
} | null;

export enum FlamechartID {
  LEFT_HEAVY = 'LEFT_HEAVY',
  CHRONO = 'CHRONO',
  SANDWICH_INVERTED_CALLERS = 'SANDWICH_INVERTED_CALLERS',
  SANDWICH_CALLEES = 'SANDWICH_CALLEES',
}

const initialFlameChartViewState: FlamechartViewState = {
  hover: null,
  selectedNode: null,
  configSpaceViewportRect: Rect.empty,
  logicalSpaceViewportSize: Vec2.zero,
};

const profileGroupState = writable<ProfileGroupState>(null);

export const activeProfile = derived(profileGroupState, $profileGroupState => {
  if ($profileGroupState == null) return null;
  return $profileGroupState.profiles[$profileGroupState?.indexToView] || null;
});

export function setProfileGroup(group: ProfileGroup) {
  profileGroupState.update(state => {
    return {
      name: group.name,
      indexToView: group.indexToView,
      profiles: group.profiles.map(p => ({
        profile: p,
        chronoViewState: initialFlameChartViewState,
        leftHeavyViewState: initialFlameChartViewState,
        sandwichViewState: { callerCallee: null },
      })),
    };
  });
}

function setProfileIndexToView(indexToView: number) {
  profileGroupState.update(state => {
    if (state == null) return state;
    indexToView = clamp(indexToView, 0, state.profiles.length - 1);
    return { ...state, indexToView };
  });
}

function updateActiveProfileState(fn: (profileState: ProfileState) => ProfileState) {
  profileGroupState.update(state => {
    if (state == null) return state;
    const { indexToView, profiles } = state;
    return {
      ...state,
      profiles: profiles.map((p, i) => (i != indexToView ? p : fn(p))),
    };
  });
}

function updateActiveSandwichViewState(
  fn: (sandwichViewState: SandwichViewState) => SandwichViewState
) {
  updateActiveProfileState(p => ({
    ...p,
    sandwichViewState: fn(p.sandwichViewState),
  }));
}

function setSelectedFrame(frame: Frame | null) {
  updateActiveSandwichViewState(sandwichViewState => {
    if (frame == null) {
      return { callerCallee: null };
    }
    return {
      callerCallee: {
        invertedCallerFlamegraph: initialFlameChartViewState,
        calleeFlamegraph: initialFlameChartViewState,
        selectedFrame: frame,
      },
    };
  });
}

function updateFlamechartState(
  id: FlamechartID,
  fn: (flamechartViewState: FlamechartViewState) => FlamechartViewState
) {
  switch (id) {
    case FlamechartID.CHRONO: {
      updateActiveProfileState(p => ({
        ...p,
        chronoViewState: fn(p.chronoViewState),
      }));
      break;
    }
    case FlamechartID.LEFT_HEAVY: {
      updateActiveProfileState(p => ({
        ...p,
        leftHeavyViewState: fn(p.leftHeavyViewState),
      }));
      break;
    }
    case FlamechartID.SANDWICH_CALLEES: {
      updateActiveSandwichViewState(s => ({
        ...s,
        callerCallee:
          s.callerCallee == null
            ? null
            : {
              ...s.callerCallee,
              calleeFlamegraph: fn(s.callerCallee.calleeFlamegraph),
            },
      }));
      break;
    }
    case FlamechartID.SANDWICH_INVERTED_CALLERS: {
      updateActiveSandwichViewState(s => ({
        ...s,
        callerCallee:
          s.callerCallee == null
            ? null
            : {
              ...s.callerCallee,
              invertedCallerFlamegraph: fn(s.callerCallee.invertedCallerFlamegraph),
            },
      }));
      break;
    }
  }
}

export function setFlamechartHoveredNode(
  id: FlamechartID,
  hover: { node: CallTreeNode; event: MouseEvent } | null
) {
  updateFlamechartState(id, f => ({ ...f, hover }));
}

export function setSelectedNode(id: FlamechartID, selectedNode: CallTreeNode | null) {
  updateFlamechartState(id, f => ({ ...f, selectedNode }));
}

export function setConfigSpaceViewportRect(id: FlamechartID, configSpaceViewportRect: Rect) {
  updateFlamechartState(id, f => ({ ...f, configSpaceViewportRect }));
}

export function setLogicalSpaceViewportSize(id: FlamechartID, logicalSpaceViewportSize: Vec2) {
  updateFlamechartState(id, f => ({ ...f, logicalSpaceViewportSize }));
}

export function useFlamechartSetters(id: FlamechartID) {
  return {
    setNodeHover: (hover: { node: CallTreeNode; event: MouseEvent } | null) =>
      setFlamechartHoveredNode(id, hover),
    setSelectedNode: (node: CallTreeNode | null) => setSelectedNode(id, node),
    setConfigSpaceViewportRect: (configSpaceViewportRect: Rect) =>
      setConfigSpaceViewportRect(id, configSpaceViewportRect),
    setLogicalSpaceViewportSize: (logicalSpaceViewportSize: Vec2) =>
      setLogicalSpaceViewportSize(id, logicalSpaceViewportSize),
  };
}

export function clearHoverNode() {
  setFlamechartHoveredNode(FlamechartID.CHRONO, null);
  setFlamechartHoveredNode(FlamechartID.LEFT_HEAVY, null);
  setFlamechartHoveredNode(FlamechartID.SANDWICH_CALLEES, null);
  setFlamechartHoveredNode(FlamechartID.SANDWICH_INVERTED_CALLERS, null);
}